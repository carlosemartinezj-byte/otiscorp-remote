//! Motor de captura de pantalla.
//!
//! Usa **DXGI Desktop Duplication** (windows-rs) sobre Direct3D 11: la GPU nos
//! entrega el frame del escritorio ya compuesto y SOLO cuando algo cambia, lo
//! que mantiene el consumo de CPU muy bajo en equipos de gama baja (Celeron/Atom).
//!
//! Arquitectura (fase actual):
//!  - Un hilo dedicado ejecuta el bucle de captura (los objetos COM de DXGI no son
//!    `Send`, asi que viven confinados en ese hilo).
//!  - El hilo mide fps, resolucion y throughput reales y publica un `CaptureStats`
//!    compartido + emite un evento Tauri `capture-stats` ~2 veces/seg para la UI.
//!  - Siguiente fase: codificar cada frame (H.264 / escala de grises segun perfil)
//!    y enviarlo por el transporte (WebRTC/socket TLS).

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Estadisticas en vivo del motor de captura (lo que la UI muestra en el panel
/// de rendimiento durante la sesion).
#[derive(Debug, Clone, Serialize, Default)]
pub struct CaptureStats {
    /// Fotogramas capturados en el ultimo segundo.
    pub fps: f32,
    pub width: u32,
    pub height: u32,
    /// Fotogramas totales desde que arranco el motor.
    pub frames: u64,
    /// Bytes del ultimo frame en crudo (BGRA), antes de codificar.
    pub last_frame_bytes: usize,
    /// Throughput estimado en crudo (MB/s) — referencia previa a codec.
    pub raw_mb_per_s: f32,
    /// El motor esta capturando.
    pub running: bool,
}

/// Consumidor de frames en crudo (BGRA empaquetado). Lo implementa el host de
/// una sesion para codificar y enviar cada frame por el transporte.
///
/// `dirty` son las regiones (x, y, w, h) que cambiaron desde el frame anterior,
/// tal como las reporta DXGI Desktop Duplication. Solo se llama cuando hay
/// contenido nuevo de verdad (un frame de "solo se movio el cursor" no genera
/// llamada: no hay nada nuevo que codificar ni enviar).
pub trait FrameSink: Send {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8], dirty: &[(u32, u32, u32, u32)]);
}

/// Controlador del motor: arranca/para el hilo y expone las estadisticas.
/// Es `Send + Sync` (solo comparte primitivas atomicas + Mutex), por lo que
/// puede vivir en el estado global de Tauri; los objetos COM quedan en el hilo.
pub struct CaptureEngine {
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<CaptureStats>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl CaptureEngine {
    pub fn new() -> Self {
        CaptureEngine {
            running: Arc::new(AtomicBool::new(false)),
            stats: Arc::new(Mutex::new(CaptureStats::default())),
            thread: Mutex::new(None),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> CaptureStats {
        self.stats.lock().unwrap().clone()
    }

    /// Arranca el bucle de captura si no estaba activo. `on_stats` se invoca
    /// ~2 veces/seg con las estadisticas; `sink` (opcional) recibe cada frame en
    /// crudo para codificarlo/enviarlo (lado host de una sesion).
    pub fn start<F>(&self, on_stats: F, sink: Option<Box<dyn FrameSink>>)
    where
        F: Fn(CaptureStats) + Send + 'static,
    {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // ya estaba corriendo
        }
        let running = self.running.clone();
        let stats = self.stats.clone();

        let handle = std::thread::Builder::new()
            .name("otiscorp-capture".into())
            .spawn(move || {
                capture_loop(running, stats, on_stats, sink);
            })
            .expect("no se pudo crear el hilo de captura");

        *self.thread.lock().unwrap() = Some(handle);
    }

    /// Detiene el bucle y espera a que el hilo termine.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        let mut s = self.stats.lock().unwrap();
        s.running = false;
        s.fps = 0.0;
        s.raw_mb_per_s = 0.0;
    }
}

impl Default for CaptureEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Entrega un frame en crudo al sink y publica estadisticas ~cada 500 ms.
/// Comun al camino DXGI y al de reserva por GDI.
#[allow(clippy::too_many_arguments)]
fn deliver_frame<F: Fn(CaptureStats)>(
    sink: &mut Option<Box<dyn FrameSink>>,
    stats: &Arc<Mutex<CaptureStats>>,
    on_stats: &F,
    width: u32,
    height: u32,
    buffer: &[u8],
    dirty: &[(u32, u32, u32, u32)],
    frames_total: &mut u64,
    window_frames: &mut u32,
    window_bytes: &mut u64,
    window_start: &mut Instant,
) {
    *frames_total += 1;
    *window_frames += 1;
    *window_bytes += buffer.len() as u64;

    if let Some(s) = sink.as_mut() {
        s.on_frame(width, height, buffer, dirty);
    }

    let elapsed = window_start.elapsed();
    if elapsed >= Duration::from_millis(500) {
        let secs = elapsed.as_secs_f32().max(0.001);
        let snap = {
            let mut s = stats.lock().unwrap();
            s.fps = *window_frames as f32 / secs;
            s.width = width;
            s.height = height;
            s.frames = *frames_total;
            s.last_frame_bytes = buffer.len();
            s.raw_mb_per_s = (*window_bytes as f32 / secs) / (1024.0 * 1024.0);
            s.running = true;
            s.clone()
        };
        on_stats(snap);
        *window_frames = 0;
        *window_bytes = 0;
        *window_start = Instant::now();
    }
}

/// Bucle comun a ambas plataformas: mide y publica estadisticas.
///
/// En Windows: DXGI Desktop Duplication es el camino eficiente, pero **no
/// entrega nada si la pantalla no cambia** (asi que recien conectado a un equipo
/// quieto la sesion se veria NEGRA) y **falla del todo en VM / RDP / equipos sin
/// monitor activo**. Para cubrir los dos casos hay una captura de reserva por
/// GDI (`win_gdi`): si DXGI no trae frame en ~700 ms — o si ni siquiera arranca —
/// se saca un frame completo por GDI. Eso garantiza el primer frame al instante
/// y una imagen (a pocos fps) siempre que haya algo que capturar.
fn capture_loop<F>(
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<CaptureStats>>,
    on_stats: F,
    mut sink: Option<Box<dyn FrameSink>>,
) where
    F: Fn(CaptureStats),
{
    // `None` si el backend nativo (DXGI en Windows) no arranca: se tira solo de
    // la reserva GDI. En plataformas sin reserva, eso es fatal.
    let mut backend: Option<Backend> = match Backend::new() {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("[captura] backend nativo no disponible ({e:?})");
            None
        }
    };

    #[cfg(windows)]
    let mut gdi = win_gdi::GdiGrabber::new();

    #[cfg(not(windows))]
    if backend.is_none() {
        eprintln!("[captura] sin backend de captura en esta plataforma");
        running.store(false, Ordering::SeqCst);
        return;
    }

    let mut frames_total: u64 = 0;
    let mut window_frames: u32 = 0;
    let mut window_bytes: u64 = 0;
    let mut window_start = Instant::now();
    // Puesto en el pasado: fuerza un primer frame (por GDI) de inmediato, aunque
    // la pantalla remota este completamente quieta.
    let mut last_delivered = Instant::now() - Duration::from_secs(1);

    while running.load(Ordering::SeqCst) {
        let mut got_native = false;

        if let Some(b) = backend.as_mut() {
            match b.next_frame(150) {
                Ok(Some(frame)) => {
                    deliver_frame(
                        &mut sink, &stats, &on_stats,
                        frame.width, frame.height, b.buffer(), &frame.dirty,
                        &mut frames_total, &mut window_frames, &mut window_bytes, &mut window_start,
                    );
                    last_delivered = Instant::now();
                    got_native = true;
                }
                Ok(None) => {}
                Err(CaptureError::AccessLost) => {
                    // Cambio de resolucion / bloqueo de sesion / fullscreen: reintentar.
                    if let Err(e) = b.reinit() {
                        eprintln!("[captura] fallo al reinicializar duplicacion: {e:?}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
                Err(CaptureError::Fatal(e)) => {
                    eprintln!("[captura] el backend nativo cayo ({e}); sigo con captura GDI de reserva");
                    backend = None;
                }
            }
        }

        // Relleno / keepalive: DXGI no trajo nada en ~700 ms (pantalla quieta o
        // DXGI caido/ausente) -> frame completo por GDI.
        if !got_native && last_delivered.elapsed() >= Duration::from_millis(700) {
            #[cfg(windows)]
            {
                match gdi.grab() {
                    Ok(frame) => {
                        deliver_frame(
                            &mut sink, &stats, &on_stats,
                            frame.width, frame.height, gdi.buffer(), &frame.dirty,
                            &mut frames_total, &mut window_frames, &mut window_bytes, &mut window_start,
                        );
                        last_delivered = Instant::now();
                    }
                    Err(e) => {
                        eprintln!("[captura] GDI de reserva fallo: {e:?}");
                        std::thread::sleep(Duration::from_millis(300));
                        if backend.is_none() {
                            // Ni DXGI ni GDI: no hay forma de capturar. Evita el
                            // busy-loop; el controlador reintentara si procede.
                            std::thread::sleep(Duration::from_millis(700));
                        }
                    }
                }
            }
            #[cfg(not(windows))]
            {
                if backend.is_none() {
                    eprintln!("[captura] sin backend y sin reserva: se detiene");
                    break;
                }
            }
        }

        // Si no hay backend nativo, marca el ritmo aqui (el keepalive GDI va a
        // ~1.4 fps; sin este sleep el bucle giraria en vacio).
        if backend.is_none() {
            std::thread::sleep(Duration::from_millis(120));
        }
    }

    running.store(false, Ordering::SeqCst);
}

/// Un frame capturado (metadatos + tamano). En esta fase medimos throughput;
/// el buffer de pixeles se reutiliza dentro del backend para no reservar por frame.
struct CapturedFrame {
    width: u32,
    height: u32,
    /// Tamano del buffer BGRA (== `backend.buffer().len()`). Se conserva como
    /// metadato aunque el bucle mida el throughput desde el propio buffer.
    #[allow(dead_code)]
    bytes: usize,
    /// Regiones (x, y, w, h) que cambiaron desde el frame anterior.
    dirty: Vec<(u32, u32, u32, u32)>,
}

#[derive(Debug)]
enum CaptureError {
    /// La duplicacion se perdio y hay que reinicializarla (recuperable).
    AccessLost,
    /// Error irrecuperable.
    Fatal(String),
}

// ===========================================================================
// Backend Windows: DXGI Desktop Duplication sobre D3D11.
// ===========================================================================
#[cfg(windows)]
mod win {
    use super::{CaptureError, CapturedFrame};
    use windows::core::Interface;
    use windows::Win32::Foundation::{HMODULE, RECT};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
        DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    };

    pub struct Backend {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output1: IDXGIOutput1,
        dupl: IDXGIOutputDuplication,
        /// Textura de staging (CPU-readable) reutilizada entre frames.
        staging: Option<ID3D11Texture2D>,
        staging_w: u32,
        staging_h: u32,
        /// Buffer BGRA empaquetado reutilizado.
        buffer: Vec<u8>,
    }

    impl Backend {
        pub fn new() -> Result<Self, CaptureError> {
            unsafe {
                let mut device: Option<ID3D11Device> = None;
                let mut context: Option<ID3D11DeviceContext> = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .map_err(|e| CaptureError::Fatal(format!("D3D11CreateDevice: {e}")))?;

                let device = device.ok_or_else(|| CaptureError::Fatal("device nulo".into()))?;
                let context = context.ok_or_else(|| CaptureError::Fatal("context nulo".into()))?;

                let (output1, dupl) = Self::make_duplication(&device)
                    .map_err(|e| CaptureError::Fatal(format!("duplicacion inicial: {e:?}")))?;

                Ok(Backend {
                    device,
                    context,
                    output1,
                    dupl,
                    staging: None,
                    staging_w: 0,
                    staging_h: 0,
                    buffer: Vec::new(),
                })
            }
        }

        /// Crea la cadena DXGI adapter -> output -> duplication para el monitor 0.
        unsafe fn make_duplication(
            device: &ID3D11Device,
        ) -> Result<(IDXGIOutput1, IDXGIOutputDuplication), windows::core::Error> {
            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter: IDXGIAdapter = dxgi_device.GetAdapter()?;
            let output: IDXGIOutput = adapter.EnumOutputs(0)?;
            let output1: IDXGIOutput1 = output.cast()?;
            let dupl: IDXGIOutputDuplication = output1.DuplicateOutput(device)?;
            Ok((output1, dupl))
        }

        /// Reinicializa la duplicacion tras un ACCESS_LOST.
        pub fn reinit(&mut self) -> Result<(), CaptureError> {
            unsafe {
                let (output1, dupl) = Self::make_duplication(&self.device)
                    .map_err(|e| CaptureError::Fatal(format!("reinit: {e:?}")))?;
                self.output1 = output1;
                self.dupl = dupl;
            }
            self.staging = None; // forzar recreacion (pudo cambiar la resolucion)
            Ok(())
        }

        /// Asegura que existe una textura de staging del tamano correcto.
        unsafe fn ensure_staging(
            &mut self,
            src: &D3D11_TEXTURE2D_DESC,
        ) -> Result<ID3D11Texture2D, CaptureError> {
            if self.staging.is_some() && self.staging_w == src.Width && self.staging_h == src.Height
            {
                return Ok(self.staging.clone().unwrap());
            }
            let mut desc = *src;
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;

            let mut staging: Option<ID3D11Texture2D> = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| CaptureError::Fatal(format!("CreateTexture2D staging: {e}")))?;
            let staging = staging.ok_or_else(|| CaptureError::Fatal("staging nulo".into()))?;

            self.staging = Some(staging.clone());
            self.staging_w = src.Width;
            self.staging_h = src.Height;
            Ok(staging)
        }

        /// Intenta obtener el siguiente frame. `Ok(None)` = sin cambios (timeout,
        /// o solo se movio el cursor sin contenido nuevo de escritorio).
        pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>, CaptureError> {
            unsafe {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;

                match self.dupl.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                    Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                        return Err(CaptureError::AccessLost)
                    }
                    Err(e) => return Err(CaptureError::Fatal(format!("AcquireNextFrame: {e}"))),
                }

                // Aseguramos liberar el frame aunque fallemos a mitad.
                let result = self.copy_frame(resource, &info);
                let _ = self.dupl.ReleaseFrame();
                result
            }
        }

        /// Lee las dirty rects de este frame (regiones de escritorio que
        /// cambiaron). Si no hay metadata disponible, asume el frame completo
        /// para no arriesgar quedarnos con contenido viejo en el buffer.
        unsafe fn read_dirty_rects(&self, width: u32, height: u32) -> Vec<(u32, u32, u32, u32)> {
            const MAX_RECTS: usize = 64;
            let mut buf = [RECT::default(); MAX_RECTS];
            let mut needed: u32 = 0;
            let cap_bytes = (MAX_RECTS * std::mem::size_of::<RECT>()) as u32;
            match self.dupl.GetFrameDirtyRects(cap_bytes, buf.as_mut_ptr(), &mut needed) {
                Ok(()) => {
                    let n = (needed as usize / std::mem::size_of::<RECT>()).min(MAX_RECTS);
                    buf[..n]
                        .iter()
                        .filter_map(|r| clamp_rect(r, width, height))
                        .collect()
                }
                Err(_) => vec![(0, 0, width, height)],
            }
        }

        unsafe fn copy_frame(
            &mut self,
            resource: Option<IDXGIResource>,
            info: &DXGI_OUTDUPL_FRAME_INFO,
        ) -> Result<Option<CapturedFrame>, CaptureError> {
            let resource = match resource {
                Some(r) => r,
                None => return Ok(None),
            };
            if info.AccumulatedFrames == 0 {
                // Solo se actualizo la posicion del cursor: sin contenido nuevo
                // de escritorio, no hay nada que codificar ni enviar.
                return Ok(None);
            }

            let tex: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| CaptureError::Fatal(format!("cast a Texture2D: {e}")))?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);

            let staging = self.ensure_staging(&desc)?;
            self.context.CopyResource(&staging, &tex);

            let width = desc.Width as usize;
            let height = desc.Height as usize;
            let tight = width * 4;

            // Si cambio el tamano del buffer (primer frame o cambio de
            // resolucion), forzamos un repintado completo esta vez: el resto
            // del buffer quedaria con basura/contenido de otra resolucion.
            let resized = self.buffer.len() != tight * height;
            if resized {
                self.buffer.resize(tight * height, 0);
            }
            let dirty = if resized {
                vec![(0, 0, desc.Width, desc.Height)]
            } else {
                self.read_dirty_rects(desc.Width, desc.Height)
            };

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Fatal(format!("Map: {e}")))?;

            let row_pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;

            // Empaquetar en BGRA contiguo, pero solo las filas que cambiaron:
            // el resto del buffer conserva el contenido del frame anterior
            // (es un lienzo persistente, no se limpia entre llamadas).
            for &(rx, ry, rw, rh) in &dirty {
                let y0 = ry as usize;
                let y1 = ((ry + rh) as usize).min(height);
                let x0 = (rx as usize) * 4;
                let x1 = (((rx + rw) as usize).min(width)) * 4;
                if x0 >= x1 || y0 >= y1 {
                    continue;
                }
                for y in y0..y1 {
                    std::ptr::copy_nonoverlapping(
                        src.add(y * row_pitch + x0),
                        self.buffer.as_mut_ptr().add(y * tight + x0),
                        x1 - x0,
                    );
                }
            }

            self.context.Unmap(&staging, 0);

            Ok(Some(CapturedFrame {
                width: desc.Width,
                height: desc.Height,
                bytes: self.buffer.len(),
                dirty,
            }))
        }

        /// Buffer BGRA empaquetado del ultimo frame capturado.
        pub fn buffer(&self) -> &[u8] {
            &self.buffer
        }
    }

    /// Recorta un RECT de DXGI (puede tener coordenadas fuera de rango en
    /// casos raros) a los limites del frame; descarta rects vacios.
    fn clamp_rect(r: &RECT, w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let x0 = r.left.max(0) as u32;
        let y0 = r.top.max(0) as u32;
        let x1 = (r.right.max(0) as u32).min(w);
        let y1 = (r.bottom.max(0) as u32).min(h);
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some((x0, y0, x1 - x0, y1 - y0))
        }
    }
}

#[cfg(windows)]
use win::Backend;

// ===========================================================================
// Reserva Windows: captura por GDI (BitBlt). Lenta y de pocos fps, pero funciona
// donde DXGI Desktop Duplication NO: maquinas virtuales, sesiones RDP, equipos
// sin monitor activo, o mientras DXGI se reinicializa. Ademas garantiza el
// PRIMER frame nada mas conectar aunque la pantalla este quieta (DXGI no entrega
// nada si no hay cambios). Modelo "pull": saca el escritorio entero por llamada.
// ===========================================================================
#[cfg(windows)]
mod win_gdi {
    use super::{CaptureError, CapturedFrame};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, ROP_CODE,
        SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    // SRCCOPY | CAPTUREBLT. CAPTUREBLT (0x4000_0000) incluye ventanas con capas.
    const CAPTUREBLT_RAW: u32 = 0x4000_0000;

    pub struct GdiGrabber {
        /// Buffer BGRA (en realidad BGRX: GDI no rellena alfa) reutilizado.
        buffer: Vec<u8>,
    }

    impl GdiGrabber {
        pub fn new() -> Self {
            GdiGrabber { buffer: Vec::new() }
        }

        /// Captura el escritorio primario completo. Devuelve un frame con dirty
        /// rect = pantalla entera (GDI no informa de regiones cambiadas).
        pub fn grab(&mut self) -> Result<CapturedFrame, CaptureError> {
            unsafe {
                let w = GetSystemMetrics(SM_CXSCREEN);
                let h = GetSystemMetrics(SM_CYSCREEN);
                if w <= 0 || h <= 0 {
                    return Err(CaptureError::Fatal("GDI: dimensiones de pantalla invalidas".into()));
                }

                let screen_dc = GetDC(HWND::default());
                if screen_dc.is_invalid() {
                    return Err(CaptureError::Fatal("GDI: GetDC devolvio nulo".into()));
                }

                let mem_dc = CreateCompatibleDC(screen_dc);
                if mem_dc.is_invalid() {
                    let _ = ReleaseDC(HWND::default(), screen_dc);
                    return Err(CaptureError::Fatal("GDI: CreateCompatibleDC nulo".into()));
                }
                let bmp = CreateCompatibleBitmap(screen_dc, w, h);
                if bmp.is_invalid() {
                    let _ = DeleteDC(mem_dc);
                    let _ = ReleaseDC(HWND::default(), screen_dc);
                    return Err(CaptureError::Fatal("GDI: CreateCompatibleBitmap nulo".into()));
                }
                let old = SelectObject(mem_dc, bmp);

                let blt = BitBlt(
                    mem_dc, 0, 0, w, h, screen_dc, 0, 0,
                    ROP_CODE(SRCCOPY.0 | CAPTUREBLT_RAW),
                );

                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = w;
                bi.bmiHeader.biHeight = -h; // negativo = filas de arriba a abajo (top-down)
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = 0; // BI_RGB

                let stride = (w as usize) * 4;
                let need = stride * (h as usize);
                if self.buffer.len() != need {
                    self.buffer.resize(need, 0);
                }

                let lines = GetDIBits(
                    mem_dc,
                    bmp,
                    0,
                    h as u32,
                    Some(self.buffer.as_mut_ptr() as *mut core::ffi::c_void),
                    &mut bi,
                    DIB_RGB_COLORS,
                );

                // Limpieza siempre (aunque algo haya fallado antes).
                let _ = SelectObject(mem_dc, old);
                let _ = DeleteObject(bmp);
                let _ = DeleteDC(mem_dc);
                let _ = ReleaseDC(HWND::default(), screen_dc);

                if blt.is_err() {
                    return Err(CaptureError::Fatal("GDI: BitBlt fallo".into()));
                }
                if lines == 0 {
                    return Err(CaptureError::Fatal("GDI: GetDIBits copio 0 lineas".into()));
                }

                Ok(CapturedFrame {
                    width: w as u32,
                    height: h as u32,
                    bytes: self.buffer.len(),
                    dirty: vec![(0, 0, w as u32, h as u32)],
                })
            }
        }

        pub fn buffer(&self) -> &[u8] {
            &self.buffer
        }
    }
}

// ===========================================================================
// Backend macOS: Core Graphics (CGDisplayCreateImage vía CGDisplay::image()).
// Modelo "pull" que graba el escritorio completo en cada llamada. Requiere el
// permiso de "Grabación de pantalla" (el sistema lo pide en el primer arranque).
// ===========================================================================
#[cfg(target_os = "macos")]
mod mac {
    use super::{CaptureError, CapturedFrame};
    use core_graphics::display::CGDisplay;
    use std::time::Duration;

    pub struct Backend {
        display: CGDisplay,
        buffer: Vec<u8>,
    }

    impl Backend {
        pub fn new() -> Result<Self, CaptureError> {
            Ok(Backend {
                display: CGDisplay::main(),
                buffer: Vec::new(),
            })
        }

        pub fn next_frame(
            &mut self,
            timeout_ms: u32,
        ) -> Result<Option<CapturedFrame>, CaptureError> {
            // Evita el busy-loop; el tope de fps real lo pone el sink por perfil.
            std::thread::sleep(Duration::from_millis(timeout_ms.min(30) as u64));

            let img = match self.display.image() {
                Some(i) => i,
                None => return Ok(None),
            };
            let w = img.width() as usize;
            let h = img.height() as usize;
            let bpr = img.bytes_per_row() as usize;
            let data = img.data();
            let bytes: &[u8] = data.bytes();
            let tight = w * 4;

            if self.buffer.len() != tight * h {
                self.buffer.resize(tight * h, 0);
            }
            for y in 0..h {
                let s = y * bpr;
                let d = y * tight;
                if s + tight <= bytes.len() {
                    self.buffer[d..d + tight].copy_from_slice(&bytes[s..s + tight]);
                }
            }

            Ok(Some(CapturedFrame {
                width: w as u32,
                height: h as u32,
                bytes: self.buffer.len(),
                // Core Graphics no reporta dirty rects: tratamos cada frame
                // como completo (sin la optimizacion de recorte de DXGI).
                dirty: vec![(0, 0, w as u32, h as u32)],
            }))
        }

        pub fn reinit(&mut self) -> Result<(), CaptureError> {
            self.display = CGDisplay::main();
            Ok(())
        }

        pub fn buffer(&self) -> &[u8] {
            &self.buffer
        }
    }
}

#[cfg(target_os = "macos")]
use mac::Backend;

// ===========================================================================
// Otras plataformas: stub para que compile.
// ===========================================================================
#[cfg(not(any(windows, target_os = "macos")))]
struct Backend;

#[cfg(not(any(windows, target_os = "macos")))]
impl Backend {
    fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Fatal(
            "captura de pantalla no implementada en esta plataforma".into(),
        ))
    }
    fn next_frame(&mut self, _timeout_ms: u32) -> Result<Option<CapturedFrame>, CaptureError> {
        Ok(None)
    }
    fn reinit(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
    fn buffer(&self) -> &[u8] {
        &[]
    }
}
