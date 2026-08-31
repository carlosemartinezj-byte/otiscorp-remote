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
pub trait FrameSink: Send {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8]);
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

/// Bucle comun a ambas plataformas: mide y publica estadisticas.
fn capture_loop<F>(
    running: Arc<AtomicBool>,
    stats: Arc<Mutex<CaptureStats>>,
    on_stats: F,
    mut sink: Option<Box<dyn FrameSink>>,
) where
    F: Fn(CaptureStats),
{
    let mut backend = match Backend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[captura] no se pudo iniciar DXGI: {e:?}");
            running.store(false, Ordering::SeqCst);
            return;
        }
    };

    let mut frames_total: u64 = 0;
    let mut window_frames: u32 = 0;
    let mut window_bytes: u64 = 0;
    let mut window_start = Instant::now();

    while running.load(Ordering::SeqCst) {
        match backend.next_frame(200) {
            Ok(Some(frame)) => {
                frames_total += 1;
                window_frames += 1;
                window_bytes += frame.bytes as u64;

                // Entregar el frame en crudo al consumidor (host de sesion).
                if let Some(s) = sink.as_mut() {
                    s.on_frame(frame.width, frame.height, backend.buffer());
                }

                // Publicar cada ~500 ms.
                let elapsed = window_start.elapsed();
                if elapsed >= Duration::from_millis(500) {
                    let secs = elapsed.as_secs_f32().max(0.001);
                    let snap = {
                        let mut s = stats.lock().unwrap();
                        s.fps = window_frames as f32 / secs;
                        s.width = frame.width;
                        s.height = frame.height;
                        s.frames = frames_total;
                        s.last_frame_bytes = frame.bytes;
                        s.raw_mb_per_s = (window_bytes as f32 / secs) / (1024.0 * 1024.0);
                        s.running = true;
                        s.clone()
                    };
                    on_stats(snap);
                    window_frames = 0;
                    window_bytes = 0;
                    window_start = Instant::now();
                }
            }
            Ok(None) => {
                // Sin cambios en pantalla (timeout DXGI): nada que hacer, no gasta CPU.
                // Aun asi refrescamos fps a la baja si llevamos rato sin frames.
                if window_start.elapsed() >= Duration::from_millis(1000) {
                    let snap = {
                        let mut s = stats.lock().unwrap();
                        s.fps = window_frames as f32; // ~0 si no hubo cambios
                        s.running = true;
                        s.clone()
                    };
                    on_stats(snap);
                    window_frames = 0;
                    window_bytes = 0;
                    window_start = Instant::now();
                }
            }
            Err(CaptureError::AccessLost) => {
                // Cambio de resolucion / bloqueo de sesion / fullscreen: reintentar.
                if let Err(e) = backend.reinit() {
                    eprintln!("[captura] fallo al reinicializar duplicacion: {e:?}");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Err(CaptureError::Fatal(e)) => {
                eprintln!("[captura] error fatal: {e}");
                break;
            }
        }
    }

    running.store(false, Ordering::SeqCst);
}

/// Un frame capturado (metadatos + tamano). En esta fase medimos throughput;
/// el buffer de pixeles se reutiliza dentro del backend para no reservar por frame.
struct CapturedFrame {
    width: u32,
    height: u32,
    bytes: usize,
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
    use windows::Win32::Foundation::HMODULE;
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

        /// Intenta obtener el siguiente frame. `Ok(None)` = sin cambios (timeout).
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
                let result = self.copy_frame(resource);
                let _ = self.dupl.ReleaseFrame();
                result
            }
        }

        unsafe fn copy_frame(
            &mut self,
            resource: Option<IDXGIResource>,
        ) -> Result<Option<CapturedFrame>, CaptureError> {
            let resource = match resource {
                Some(r) => r,
                None => return Ok(None),
            };
            let tex: ID3D11Texture2D = resource
                .cast()
                .map_err(|e| CaptureError::Fatal(format!("cast a Texture2D: {e}")))?;

            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);

            let staging = self.ensure_staging(&desc)?;
            self.context.CopyResource(&staging, &tex);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Fatal(format!("Map: {e}")))?;

            let width = desc.Width as usize;
            let height = desc.Height as usize;
            let row_pitch = mapped.RowPitch as usize;
            let tight = width * 4;

            // Empaquetar en BGRA contiguo (sin el padding de fila del staging).
            if self.buffer.len() != tight * height {
                self.buffer.resize(tight * height, 0);
            }
            let src = mapped.pData as *const u8;
            for y in 0..height {
                std::ptr::copy_nonoverlapping(
                    src.add(y * row_pitch),
                    self.buffer.as_mut_ptr().add(y * tight),
                    tight,
                );
            }

            self.context.Unmap(&staging, 0);

            Ok(Some(CapturedFrame {
                width: desc.Width,
                height: desc.Height,
                bytes: self.buffer.len(),
            }))
        }

        /// Buffer BGRA empaquetado del ultimo frame capturado.
        pub fn buffer(&self) -> &[u8] {
            &self.buffer
        }
    }
}

#[cfg(windows)]
use win::Backend;

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
