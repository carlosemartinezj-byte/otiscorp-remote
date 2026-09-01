//! Transporte de sesion remota (compartir pantalla + control).
//!
//! Dos vias de conexion, mismo protocolo de aplicacion:
//!  - **LAN**: descubrimiento por ID (UDP broadcast) + dos sockets TCP directos
//!    (video y entrada) entre host y visor.
//!  - **Internet**: los mismos dos sockets, pero cada uno tunelado por el relay
//!    (`relay.rs`) contra `otiscorp-relay.fly.dev`, emparejados por ID.
//!
//! **Dos sockets, no uno.** El video (frames grandes, unidireccional host->visor)
//! y la entrada (raton/teclado, pequena y bidireccional) van cada uno por su
//! propia conexion TCP. Antes compartian una sola conexion multiplexada por tipo
//! de mensaje: un frame de cientos de KB escribiendose retrasaba el clic que iba
//! justo detras. Separarlos elimina ese head-of-line blocking sin tocar el
//! relay (los dos sockets de una misma sesion internet solo se distinguen por
//! un digito de sufijo en el ID que usan para emparejarse).
//!
//! **Codec**: H.264 (Media Foundation, ver `h264enc.rs`) con fallback a MJPEG
//! si el equipo no tiene ningun encoder H.264 disponible (no deberia pasar en
//! Windows 8+, pero cubre macOS y cualquier caso raro). El primer byte de cada
//! frame en el wire dice cual es.
//!
//! **Cola de un solo hueco**: el hilo de captura nunca escribe en el socket.
//! Solo dejan el frame codificado en un buzon de un hueco; un hilo aparte lo
//! escribe. Si ese hilo no ha vaciado el hueco cuando llega el siguiente frame,
//! la captura se salta la codificacion de ESE frame en vez de encolarlo: bajo
//! congestion se pierde fps, no se acumula retraso. Las dirty rects de los
//! frames saltados se guardan y se aplican en el proximo frame que si se manda.
//!
//! **Bitrate adaptativo**: el hilo escritor mide cuantos frames se saltaron por
//! saturacion frente a los que sí se mandaron y sube o baja el bitrate objetivo
//! del encoder cada ~1.5s dentro del rango del perfil elegido.

use crate::capture::{CaptureEngine, CaptureStats, FrameSink};
use crate::input;
use base64::Engine;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const DISCOVERY_PORT: u16 = 49321;
const SESSION_PORT: u16 = 49322; // video (host -> visor)
const INPUT_PORT: u16 = 49323; // entrada/control (visor <-> host)

const MSG_FRAME: u8 = 0x01;
const MSG_INPUT: u8 = 0x10;

/// Buffer de socket para el canal de VIDEO. Pequeño a propósito: con TCP,
/// `write_all` no bloquea hasta que el buffer del SO se llena, así que un buffer
/// grande deja que se apilen 5-10 frames = 1-2 s de retraso. Con ~96 KB solo
/// caben ~2 frames; en cuanto se llena, el escritor se bloquea, el `FrameQueue`
/// marca "busy" y el host DESCARTA frames en vez de acumular retraso.
const VIDEO_SOCK_BUF: usize = 96 * 1024;

/// Acota el retraso de un socket de video: TCP_NODELAY + buffers de envío/recepción
/// pequeños para que no se acumulen frames en vuelo.
pub(crate) fn tune_video_socket(stream: &TcpStream, send: bool, recv: bool) {
    let s = socket2::SockRef::from(stream);
    let _ = s.set_nodelay(true);
    if send {
        let _ = s.set_send_buffer_size(VIDEO_SOCK_BUF);
    }
    if recv {
        let _ = s.set_recv_buffer_size(VIDEO_SOCK_BUF);
    }
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(10));
    let _ = s.set_tcp_keepalive(&ka);
}

/// H.264 (Media Foundation) da mejor calidad por bit, pero depende de que el MFT
/// del host codifique sin fallar Y de que el WebView del visor traiga WebCodecs
/// (`VideoDecoder`). Cualquiera de los dos fallando = pantalla negra silenciosa.
/// Mientras se estabiliza, el transporte usa **MJPEG**: simple, sin dependencias
/// del visor, a prueba de todo. Poner a `true` para volver a intentar H.264.
const USE_H264: bool = false;

// Codec del payload de un MSG_FRAME (primer byte).
const CODEC_JPEG: u8 = 0;
const CODEC_H264: u8 = 1;
/// JPEG por celdas: en vez de re-mandar la pantalla entera cada frame, solo las
/// celdas de una rejilla que han cambiado (DXGI nos da las "dirty rects"). Un
/// frame de "escribiendo en el editor" pasa de ~250 KB a ~3 KB -> nitidez
/// completa Y fluidez por el relay. Cada ~2 s (o al cambiar de resolucion) se
/// manda una celda unica con la pantalla entera = keyframe, para recuperar.
/// Body: [n:u16 BE] + n x [x:u16][y:u16][w:u16][h:u16][len:u32 BE][jpeg...]
const CODEC_JPEG_TILES: u8 = 2;

/// Lado de la celda de la rejilla de cambios, en pixeles.
const TILE_SIZE: u32 = 160;
/// Cada cuanto se manda un frame completo (keyframe) aunque no toque.
const TILES_KEYFRAME_EVERY: Duration = Duration::from_millis(2000);
/// Si cambia mas de este % de la pantalla, sale mas barato mandar un keyframe
/// que muchas celdas sueltas.
const TILES_KEYFRAME_PCT: u32 = 55;

// ---- Framing por mensajes: [u8 tipo][u32 be longitud][payload] -------------

fn write_msg(stream: &mut TcpStream, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = (payload.len() as u32).to_be_bytes();
    stream.write_all(&[kind])?;
    stream.write_all(&len)?;
    stream.write_all(payload)?;
    Ok(())
}

fn read_msg(stream: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    stream.read_exact(&mut hdr)?;
    let kind = hdr[0];
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok((kind, buf))
}

/// Empaqueta un frame codificado en el formato del wire:
/// `[codec:u8][flags:u8][w:u32 be][h:u32 be][datos...]`. `flags` bit0 = keyframe.
fn build_frame_payload(codec: u8, keyframe: bool, w: u32, h: u32, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(10 + data.len());
    payload.push(codec);
    payload.push(if keyframe { 1 } else { 0 });
    payload.extend_from_slice(&w.to_be_bytes());
    payload.extend_from_slice(&h.to_be_bytes());
    payload.extend_from_slice(data);
    payload
}

// ---- Perfil de calidad JPEG (fallback) -------------------------------------

struct JpegQuality {
    jpeg: u8,
    scale: u32,
    min_interval: Duration,
}

fn jpeg_quality_for(profile: &str) -> JpegQuality {
    match profile {
        // Con envio por celdas, la resolucion completa ya sale barata: solo se
        // manda lo que cambia. "Nitido" y "Equilibrado" van los dos a resolucion
        // nativa y se diferencian en la calidad del JPEG.
        "sharp" => JpegQuality { jpeg: 80, scale: 1, min_interval: Duration::from_millis(25) },
        "balanced" => JpegQuality { jpeg: 68, scale: 1, min_interval: Duration::from_millis(30) },
        // Ultraligero: media resolucion (sigue el camino de frame completo, sin
        // celdas). Para enlaces muy flojos / CPUs muy justas.
        _ => JpegQuality { jpeg: 58, scale: 2, min_interval: Duration::from_millis(40) },
    }
}

/// Calidad para la ruta WebRTC (internet, `LocalFrameSink`). Mas agresiva que la
/// de LAN: el video va TROCEADO por un data channel y cada frame compite con el
/// ancho de banda de SUBIDA real del host. Frames mas pequenos = menos trozos =
/// mas robusto y sin saturar el enlace. Se pierde nitidez pero la imagen llega
/// fluida en vez de quedarse negra.
fn jpeg_quality_for_rtc(profile: &str) -> JpegQuality {
    match profile {
        // Nitido: resolucion completa, pero calidad/fps contenidos.
        "sharp" => JpegQuality { jpeg: 68, scale: 1, min_interval: Duration::from_millis(66) },
        // Equilibrado / Ultraligero: media resolucion (clave para que el JPEG
        // quepa en pocos KB por internet).
        "balanced" => JpegQuality { jpeg: 62, scale: 2, min_interval: Duration::from_millis(66) },
        _ => JpegQuality { jpeg: 55, scale: 2, min_interval: Duration::from_millis(100) },
    }
}

/// Reduce la resolucion por muestreo (nearest) a 1/scale. Devuelve (w,h) nuevos.
fn downscale_into(src: &[u8], w: u32, h: u32, scale: u32, out: &mut Vec<u8>) -> (u32, u32) {
    let w = w as usize;
    let h = h as usize;
    let scale = scale as usize;
    let sw = (w / scale).max(1);
    let sh = (h / scale).max(1);
    out.resize(sw * sh * 4, 0);
    for y in 0..sh {
        let sy = y * scale;
        for x in 0..sw {
            let sx = x * scale;
            let si = (sy * w + sx) * 4;
            let di = (y * sw + x) * 4;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    (sw as u32, sh as u32)
}

// ---- Sink del host (MJPEG): codifica cada frame a JPEG completo ----------
//
// **El hilo de captura NUNCA escribe en el socket.** Codifica el frame y lo deja
// en un buzon de un solo hueco; un hilo aparte hace la escritura de red
// (bloqueante). Si la red va detras cuando llega el siguiente frame, se DESCARTA
// el que estaba en el hueco (gana el mas reciente) en vez de encolar: bajo
// congestion se pierde fps, no se acumula retraso. Antes `on_frame` escribia
// directo en el socket, asi que la captura se quedaba parada ~1 frame entero
// esperando a que el JPEG cruzara el relay -> 3 fps.

/// Anexa un bloque de celda al body: [x:u16][y:u16][w:u16][h:u16][len:u32][jpeg].
fn push_tile(out: &mut Vec<u8>, x: u16, y: u16, w: u16, h: u16, jpeg: &[u8]) {
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out.extend_from_slice(&w.to_be_bytes());
    out.extend_from_slice(&h.to_be_bytes());
    out.extend_from_slice(&(jpeg.len() as u32).to_be_bytes());
    out.extend_from_slice(jpeg);
}

struct JpegSender {
    queue: Arc<FrameQueue>,
    alive: Arc<AtomicBool>,
    quality: JpegQuality,
    last_sent: Instant,
    last_keyframe: Instant,
    // Dimensiones del ultimo frame visto (para detectar cambio de resolucion).
    fw: u32,
    fh: u32,
    // Rejilla de celdas cambiadas, ACUMULADA entre frames que se saltan: si nos
    // saltamos un envio (pacing / red ocupada) los cambios de ese frame no se
    // pierden, quedan marcados aqui hasta el siguiente envio real.
    grid_cols: u32,
    grid_rows: u32,
    cells: Vec<bool>,
    cells_dirty: bool,
    // Scratch reutilizado entre frames.
    scaled: Vec<u8>,
    tile_bgra: Vec<u8>,
    enc_buf: Vec<u8>,
}

impl JpegSender {
    fn new(stream: TcpStream, quality: JpegQuality, alive: Arc<AtomicBool>) -> Self {
        let queue = FrameQueue::new();
        spawn_simple_frame_writer(stream, queue.clone(), alive.clone());
        JpegSender {
            queue,
            alive,
            quality,
            last_sent: Instant::now() - Duration::from_secs(1),
            last_keyframe: Instant::now() - Duration::from_secs(60),
            fw: 0,
            fh: 0,
            grid_cols: 0,
            grid_rows: 0,
            cells: Vec::new(),
            cells_dirty: false,
            scaled: Vec::new(),
            tile_bgra: Vec::new(),
            enc_buf: Vec::new(),
        }
    }

    /// Codifica `bgra` (w x h) a JPEG 4:2:0 en `self.enc_buf`. `true` si ok.
    fn encode_into_buf(&mut self, w: u32, h: u32, bgra: &[u8]) -> bool {
        self.enc_buf.clear();
        let mut enc = jpeg_encoder::Encoder::new(&mut self.enc_buf, self.quality.jpeg);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
        enc.encode(bgra, w as u16, h as u16, jpeg_encoder::ColorType::Bgra)
            .is_ok()
    }

    /// Marca en la rejilla las celdas que toca el rect (x, y, w, h).
    fn mark_cells(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 || self.grid_cols == 0 {
            return;
        }
        let x1 = (x + w).min(self.fw);
        let y1 = (y + h).min(self.fh);
        if x1 <= x || y1 <= y {
            return;
        }
        let cx0 = x / TILE_SIZE;
        let cy0 = y / TILE_SIZE;
        let cx1 = ((x1 - 1) / TILE_SIZE).min(self.grid_cols - 1);
        let cy1 = ((y1 - 1) / TILE_SIZE).min(self.grid_rows - 1);
        for cy in cy0..=cy1 {
            for cx in cx0..=cx1 {
                self.cells[(cy * self.grid_cols + cx) as usize] = true;
            }
        }
        self.cells_dirty = true;
    }
}

impl FrameSink for JpegSender {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8], dirty: &[(u32, u32, u32, u32)]) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        if bgra.len() < (width as usize * height as usize * 4) {
            return;
        }

        // ---- Ultraligero: frame completo a media resolucion, sin celdas ----
        if self.quality.scale > 1 {
            if self.last_sent.elapsed() < self.quality.min_interval || self.queue.is_busy() {
                return;
            }
            let (sw, sh) =
                downscale_into(bgra, width, height, self.quality.scale, &mut self.scaled);
            let scaled = std::mem::take(&mut self.scaled);
            let ok = self.encode_into_buf(sw, sh, &scaled);
            self.scaled = scaled;
            if !ok {
                return;
            }
            let payload = build_frame_payload(CODEC_JPEG, true, sw, sh, &self.enc_buf);
            self.queue.push(payload);
            self.last_sent = Instant::now();
            return;
        }

        // ---- Nitido / Equilibrado: resolucion nativa, solo celdas cambiadas ----
        let sw = width;
        let sh = height;

        // (Re)dimensionar la rejilla si cambio la resolucion -> fuerza keyframe.
        let mut force_keyframe = false;
        if self.fw != sw || self.fh != sh {
            self.fw = sw;
            self.fh = sh;
            self.grid_cols = (sw + TILE_SIZE - 1) / TILE_SIZE;
            self.grid_rows = (sh + TILE_SIZE - 1) / TILE_SIZE;
            self.cells = vec![false; (self.grid_cols * self.grid_rows) as usize];
            self.cells_dirty = false;
            force_keyframe = true;
        }

        // Acumular las dirty rects de ESTE frame en la rejilla.
        if dirty.is_empty() {
            force_keyframe = true;
        } else {
            for &(rx, ry, rw, rh) in dirty {
                self.mark_cells(rx, ry, rw, rh);
            }
        }

        // Pacing + backpressure: si no toca por tiempo o la red va ocupada, los
        // cambios quedan marcados y volvemos sin enviar.
        let keyframe_due = self.last_keyframe.elapsed() >= TILES_KEYFRAME_EVERY;
        if !force_keyframe {
            if self.queue.is_busy() {
                return;
            }
            if self.last_sent.elapsed() < self.quality.min_interval {
                return;
            }
            if !self.cells_dirty && !keyframe_due {
                return; // nada nuevo que mandar
            }
        } else if self.queue.is_busy() {
            return;
        }

        let ncells = (self.grid_cols * self.grid_rows) as usize;
        let ndirty = self.cells.iter().filter(|&&b| b).count();
        let keyframe = force_keyframe
            || keyframe_due
            || (ndirty as u32) * 100 >= (ncells as u32) * TILES_KEYFRAME_PCT;

        let mut body: Vec<u8> = Vec::new();
        let mut n: u16 = 0;

        if keyframe {
            if !self.encode_into_buf(sw, sh, bgra) {
                return;
            }
            push_tile(&mut body, 0, 0, sw as u16, sh as u16, &self.enc_buf);
            n = 1;
            self.last_keyframe = Instant::now();
        } else {
            let cols = self.grid_cols;
            let rows = self.grid_rows;
            for cy in 0..rows {
                let mut cx = 0u32;
                while cx < cols {
                    if !self.cells[(cy * cols + cx) as usize] {
                        cx += 1;
                        continue;
                    }
                    // Fusiona celdas contiguas de la fila en un tile mas ancho
                    // (ahorra cabeceras JPEG).
                    let run_start = cx;
                    while cx < cols && self.cells[(cy * cols + cx) as usize] {
                        cx += 1;
                    }
                    let px = run_start * TILE_SIZE;
                    let py = cy * TILE_SIZE;
                    let pw = (cx * TILE_SIZE).min(sw) - px;
                    let ph = ((cy + 1) * TILE_SIZE).min(sh) - py;

                    let need = (pw as usize) * (ph as usize) * 4;
                    if self.tile_bgra.len() < need {
                        self.tile_bgra.resize(need, 0);
                    }
                    let row_bytes = (pw as usize) * 4;
                    for row in 0..ph as usize {
                        let s = ((py as usize + row) * (sw as usize) + px as usize) * 4;
                        let d = row * row_bytes;
                        self.tile_bgra[d..d + row_bytes]
                            .copy_from_slice(&bgra[s..s + row_bytes]);
                    }

                    let tile = std::mem::take(&mut self.tile_bgra);
                    let ok = self.encode_into_buf(pw, ph, &tile[..need]);
                    self.tile_bgra = tile;
                    if ok {
                        push_tile(
                            &mut body,
                            px as u16,
                            py as u16,
                            pw as u16,
                            ph as u16,
                            &self.enc_buf,
                        );
                        n += 1;
                    }
                }
            }
            if n == 0 {
                return;
            }
        }

        let mut framed = Vec::with_capacity(2 + body.len());
        framed.extend_from_slice(&n.to_be_bytes());
        framed.extend_from_slice(&body);
        let payload = build_frame_payload(CODEC_JPEG_TILES, keyframe, sw, sh, &framed);
        self.queue.push(payload);
        self.last_sent = Instant::now();

        for b in self.cells.iter_mut() {
            *b = false;
        }
        self.cells_dirty = false;
    }
}

/// Hilo escritor minimo para MJPEG: saca el frame mas reciente del buzon y lo
/// escribe en el socket (bloqueante). Sin logica de bitrate (eso es solo del
/// camino H.264). Termina cuando `alive` se apaga o falla la escritura.
fn spawn_simple_frame_writer(mut stream: TcpStream, queue: Arc<FrameQueue>, alive: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("otis-jpeg-writer".into())
        .spawn(move || loop {
            if !alive.load(Ordering::SeqCst) {
                break;
            }
            if let Some(payload) = queue.pop_timeout(Duration::from_millis(500)) {
                if write_msg(&mut stream, MSG_FRAME, &payload).is_err() {
                    alive.store(false, Ordering::SeqCst);
                    queue.shutdown();
                    break;
                }
            } else if !queue.alive.load(Ordering::SeqCst) {
                break;
            }
        })
        .ok();
}

// ---- Cola de un solo hueco (frame mas reciente, descarta el anterior) -----

struct FrameQueue {
    slot: Mutex<Option<Vec<u8>>>,
    cv: Condvar,
    alive: AtomicBool,
    sent_frames: std::sync::atomic::AtomicU64,
    dropped_frames: std::sync::atomic::AtomicU64,
}

impl FrameQueue {
    fn new() -> Arc<Self> {
        Arc::new(FrameQueue {
            slot: Mutex::new(None),
            cv: Condvar::new(),
            alive: AtomicBool::new(true),
            sent_frames: std::sync::atomic::AtomicU64::new(0),
            dropped_frames: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// true si el hueco todavia tiene un frame sin escribir: la red va detras.
    fn is_busy(&self) -> bool {
        self.slot.lock().unwrap().is_some()
    }

    fn push(&self, data: Vec<u8>) {
        let mut g = self.slot.lock().unwrap();
        if g.is_some() {
            self.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
        *g = Some(data);
        drop(g);
        self.cv.notify_one();
    }

    /// Espera hasta `dur` por un frame. `None` = nada nuevo (timeout o cierre).
    fn pop_timeout(&self, dur: Duration) -> Option<Vec<u8>> {
        let mut g = self.slot.lock().unwrap();
        if g.is_none() {
            let (guard, _) = self.cv.wait_timeout(g, dur).unwrap();
            g = guard;
        }
        g.take()
    }

    fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.cv.notify_all();
    }
}

/// Hilo dedicado a escribir en el socket de video: el hilo de captura nunca
/// bloquea en I/O de red. Tambien mide la salud de la conexion (frames
/// saltados por saturacion vs. enviados) y ajusta el bitrate objetivo.
#[cfg(windows)]
fn spawn_frame_writer(
    mut stream: TcpStream,
    queue: Arc<FrameQueue>,
    alive: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    bitrate_min: u32,
    bitrate_max: u32,
) {
    std::thread::Builder::new()
        .name("otis-frame-writer".into())
        .spawn(move || {
            let mut last_tick = Instant::now();
            let mut last_sent: u64 = 0;
            let mut last_dropped: u64 = 0;
            loop {
                if !alive.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(payload) = queue.pop_timeout(Duration::from_millis(500)) {
                    if write_msg(&mut stream, MSG_FRAME, &payload).is_err() {
                        alive.store(false, Ordering::SeqCst);
                        queue.shutdown();
                        break;
                    }
                    queue.sent_frames.fetch_add(1, Ordering::Relaxed);
                } else if !queue.alive.load(Ordering::SeqCst) {
                    break;
                }

                if last_tick.elapsed() >= Duration::from_millis(1500) {
                    let sent = queue.sent_frames.load(Ordering::Relaxed);
                    let dropped = queue.dropped_frames.load(Ordering::Relaxed);
                    let d_sent = sent.saturating_sub(last_sent);
                    let d_dropped = dropped.saturating_sub(last_dropped);
                    last_sent = sent;
                    last_dropped = dropped;
                    let total = d_sent + d_dropped;
                    if total > 0 {
                        let drop_ratio = d_dropped as f32 / total as f32;
                        let cur = target_bitrate.load(Ordering::Relaxed);
                        let next = if drop_ratio > 0.15 {
                            (cur as f32 * 0.75) as u32 // la red no aguanta: bajamos rapido
                        } else if drop_ratio == 0.0 {
                            (cur as f32 * 1.10) as u32 // sobra margen: subimos con cautela
                        } else {
                            cur
                        };
                        target_bitrate.store(next.clamp(bitrate_min, bitrate_max), Ordering::Relaxed);
                    }
                    last_tick = Instant::now();
                }
            }
        })
        .ok();
}

struct VideoQuality {
    scale: u32,
    fps: u32,
    min_interval: Duration,
    bitrate_min: u32,
    bitrate_start: u32,
    bitrate_max: u32,
}

fn video_quality_for(profile: &str) -> VideoQuality {
    match profile {
        "sharp" => VideoQuality {
            scale: 1,
            fps: 30,
            min_interval: Duration::from_millis(33),
            bitrate_min: 1_000_000,
            bitrate_start: 3_000_000,
            bitrate_max: 8_000_000,
        },
        "balanced" => VideoQuality {
            scale: 1,
            fps: 30,
            min_interval: Duration::from_millis(33),
            bitrate_min: 500_000,
            bitrate_start: 1_800_000,
            bitrate_max: 4_000_000,
        },
        // ultraligero: media resolucion, algo menos de fps, bitrate bajo — pensado
        // para redes flojas y CPUs de gama baja.
        _ => VideoQuality {
            scale: 2,
            fps: 24,
            min_interval: Duration::from_millis(42),
            bitrate_min: 200_000,
            bitrate_start: 700_000,
            bitrate_max: 1_500_000,
        },
    }
}

/// Sink del host: codifica a H.264 (Media Foundation) reutilizando un lienzo
/// NV12 persistente que solo se repinta en las dirty rects de cada frame.
#[cfg(windows)]
struct H264Sender {
    queue: Arc<FrameQueue>,
    alive: Arc<AtomicBool>,
    force_keyframe: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    current_bitrate: u32,
    quality: VideoQuality,
    encoder: Option<crate::h264enc::H264Encoder>,
    canvas: Vec<u8>,
    canvas_w: u32,
    canvas_h: u32,
    pending_dirty: Vec<(u32, u32, u32, u32)>,
    scaled: Vec<u8>,
    last_encode: Instant,
}

// SAFETY: `H264Sender` se construye en el hilo que acepta la conexion (con
// `encoder: None`, sin ningun objeto COM todavia) y el `Box<dyn FrameSink>`
// resultante se mueve UNA sola vez al hilo de captura recien creado, que es el
// unico que a partir de ahi llama a `on_frame` (donde se crea el `IMFTransform`
// de verdad) y el unico que lo suelta (al terminar `capture_loop`, en el mismo
// hilo). Nunca hay dos hilos tocando el encoder a la vez, que es lo que MF
// exige realmente; `windows-rs` solo es conservador por defecto con `Send`
// para interfaces COM.
#[cfg(windows)]
unsafe impl Send for H264Sender {}

#[cfg(windows)]
impl H264Sender {
    fn new(stream: TcpStream, profile: &str, alive: Arc<AtomicBool>, force_keyframe: Arc<AtomicBool>) -> Self {
        let quality = video_quality_for(profile);
        let queue = FrameQueue::new();
        let target_bitrate = Arc::new(AtomicU32::new(quality.bitrate_start));
        spawn_frame_writer(
            stream,
            queue.clone(),
            alive.clone(),
            target_bitrate.clone(),
            quality.bitrate_min,
            quality.bitrate_max,
        );
        H264Sender {
            queue,
            alive,
            force_keyframe,
            current_bitrate: quality.bitrate_start,
            target_bitrate,
            quality,
            encoder: None,
            canvas: Vec::new(),
            canvas_w: 0,
            canvas_h: 0,
            pending_dirty: Vec::new(),
            scaled: Vec::new(),
            last_encode: Instant::now() - Duration::from_secs(1),
        }
    }
}

#[cfg(windows)]
impl FrameSink for H264Sender {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8], dirty: &[(u32, u32, u32, u32)]) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        if bgra.len() < (width as usize * height as usize * 4) {
            return;
        }

        let want_keyframe_now = self.force_keyframe.swap(false, Ordering::AcqRel);

        // Tope de fps del perfil: si no toca aun, acumulamos las dirty rects
        // para cuando si nos toque codificar y salimos sin gastar CPU.
        if self.last_encode.elapsed() < self.quality.min_interval && !want_keyframe_now {
            self.pending_dirty.extend_from_slice(dirty);
            return;
        }

        let (sw, sh): (u32, u32) = if self.quality.scale <= 1 {
            (width, height)
        } else {
            downscale_into(bgra, width, height, self.quality.scale, &mut self.scaled)
        };

        // (Re)crear encoder y lienzo si es la primera vez o cambio el tamano
        // (p. ej. el equipo remoto cambio de resolucion).
        let need_full_repaint = self.encoder.is_none() || self.canvas_w != sw || self.canvas_h != sh;
        if need_full_repaint {
            match crate::h264enc::H264Encoder::new(sw, sh, self.quality.fps, self.current_bitrate) {
                Ok(enc) => {
                    self.encoder = Some(enc);
                    self.canvas = vec![0u8; crate::h264enc::nv12_size(sw as usize, sh as usize)];
                    self.canvas_w = sw;
                    self.canvas_h = sh;
                }
                Err(e) => {
                    eprintln!("[h264] no se pudo crear el encoder ({sw}x{sh}): {e}");
                    self.alive.store(false, Ordering::SeqCst);
                    self.queue.shutdown();
                    return;
                }
            }
        }

        // Bajo saturacion de red nos saltamos ESTE frame (ni se codifica): las
        // dirty rects se acumulan para cuando el escritor vacie el hueco.
        if self.queue.is_busy() {
            self.pending_dirty.extend_from_slice(dirty);
            return;
        }

        let effective_dirty: Vec<(u32, u32, u32, u32)> = if need_full_repaint || self.quality.scale > 1 {
            vec![(0, 0, sw, sh)]
        } else if self.pending_dirty.is_empty() {
            dirty.to_vec()
        } else {
            let mut all = std::mem::take(&mut self.pending_dirty);
            all.extend_from_slice(dirty);
            all
        };
        self.pending_dirty.clear();

        let src: &[u8] = if self.quality.scale <= 1 { bgra } else { &self.scaled };
        crate::h264enc::patch_nv12_from_bgra(&mut self.canvas, sw as usize, sh as usize, src, &effective_dirty);

        let wanted_bitrate = self.target_bitrate.load(Ordering::Relaxed);
        if wanted_bitrate != self.current_bitrate {
            if let Some(enc) = self.encoder.as_mut() {
                enc.set_bitrate(wanted_bitrate);
            }
            self.current_bitrate = wanted_bitrate;
        }
        if want_keyframe_now {
            if let Some(enc) = self.encoder.as_mut() {
                enc.request_keyframe();
            }
        }

        let encoded = match self.encoder.as_mut().unwrap().encode(&self.canvas) {
            Ok(frames) => frames,
            Err(e) => {
                eprintln!("[h264] error al codificar: {e}");
                self.alive.store(false, Ordering::SeqCst);
                self.queue.shutdown();
                return;
            }
        };
        for f in encoded {
            let payload = build_frame_payload(CODEC_H264, f.keyframe, sw, sh, &f.data);
            self.queue.push(payload);
        }
        self.last_encode = Instant::now();
    }
}

/// Elige el sink de video: H.264 si el equipo tiene algun encoder disponible,
/// si no MJPEG (macOS, o el raro caso de un Windows sin ningun MFT H.264).
fn make_video_sink(
    stream: TcpStream,
    profile: &str,
    alive: Arc<AtomicBool>,
    #[allow(unused_variables)] force_keyframe: Arc<AtomicBool>,
) -> Box<dyn FrameSink> {
    #[cfg(windows)]
    {
        if USE_H264 && crate::h264enc::is_available() {
            return Box::new(H264Sender::new(stream, profile, alive, force_keyframe));
        }
    }
    Box::new(JpegSender::new(stream, jpeg_quality_for(profile), alive))
}

// ---- Sink local: emite cada frame al PROPIO WebView (para reenviar por WebRTC).
// Lo usa el modo P2P por internet (webrtc.js, ruta separada de la de arriba):
// el capturador entrega el frame, aqui se codifica a JPEG y se emite el evento
// `local-frame`; el frontend lo manda por el data channel de WebRTC al visor.
// Sigue en MJPEG: WebRTC no aparece en el alcance de esta migracion a H.264
// (esa ruta no pasa por `handle_incoming`/los dos sockets de arriba).

struct LocalFrameSink {
    app: AppHandle,
    quality: JpegQuality,
    last_sent: Instant,
    scaled: Vec<u8>,
    enc_buf: Vec<u8>,
}

impl FrameSink for LocalFrameSink {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8], _dirty: &[(u32, u32, u32, u32)]) {
        if self.last_sent.elapsed() < self.quality.min_interval {
            return;
        }
        if bgra.len() < (width as usize * height as usize * 4) {
            return;
        }
        let (sw, sh, data): (u32, u32, &[u8]) = if self.quality.scale <= 1 {
            (width, height, bgra)
        } else {
            let (sw, sh) = downscale_into(bgra, width, height, self.quality.scale, &mut self.scaled);
            (sw, sh, &self.scaled[..])
        };

        self.enc_buf.clear();
        let encoder = jpeg_encoder::Encoder::new(&mut self.enc_buf, self.quality.jpeg);
        if encoder
            .encode(data, sw as u16, sh as u16, jpeg_encoder::ColorType::Bgra)
            .is_err()
        {
            return;
        }

        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.enc_buf);
        let _ = self.app.emit(
            "local-frame",
            serde_json::json!({ "jpeg": b64, "width": sw, "height": sh }),
        );
        self.last_sent = Instant::now();
    }
}

/// Crea un sink que emite los frames al propio WebView segun el perfil dado.
/// Usa la tabla de calidad especifica de WebRTC (frames pequenos para que
/// quepan troceados en el data channel sin saturar la subida).
pub fn make_local_sink(app: AppHandle, profile: &str) -> Box<dyn FrameSink> {
    Box::new(LocalFrameSink {
        app,
        quality: jpeg_quality_for_rtc(profile),
        last_sent: Instant::now() - Duration::from_secs(1),
        scaled: Vec::new(),
        enc_buf: Vec::new(),
    })
}

// ===========================================================================
// Transport: estado compartido (Send + Sync) que vive en el estado de Tauri.
// ===========================================================================

pub struct Transport {
    self_id: Mutex<String>,
    incoming_active: Arc<AtomicBool>,
    viewer: Mutex<Option<ViewerHandle>>,
    pending_decision: Mutex<Option<std::sync::mpsc::Sender<bool>>>,
    // Handles para poder cortar una sesion entrante (video, entrada) desde ESTE
    // equipo (el que esta siendo controlado).
    incoming_kill: Mutex<Option<(TcpStream, TcpStream)>>,
}

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(20);

struct ViewerHandle {
    input_write: TcpStream, // handle de escritura del canal de entrada
    video_kill: TcpStream, // solo para poder cerrar el canal de video al desconectar
    running: Arc<AtomicBool>,
}

impl Transport {
    pub fn new() -> Self {
        Transport {
            self_id: Mutex::new(String::new()),
            incoming_active: Arc::new(AtomicBool::new(false)),
            viewer: Mutex::new(None),
            pending_decision: Mutex::new(None),
            incoming_kill: Mutex::new(None),
        }
    }

    /// Responde a una solicitud entrante pendiente (llamado desde el dialogo
    /// de aprobacion del frontend). Si no hay ninguna pendiente, no hace nada.
    pub fn respond_incoming(&self, accept: bool) {
        if let Some(tx) = self.pending_decision.lock().unwrap().take() {
            let _ = tx.send(accept);
        }
    }

    /// Corta la sesion entrante activa (LAN o internet) desde ESTE equipo (el
    /// que esta siendo controlado). Cierra ambos sockets, lo que desbloquea la
    /// lectura y termina `handle_incoming` de forma normal.
    pub fn end_incoming(&self) {
        if let Some((video, input)) = self.incoming_kill.lock().unwrap().take() {
            let _ = video.shutdown(std::net::Shutdown::Both);
            let _ = input.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Arranca el host (acceso desatendido): responde al descubrimiento y acepta
    /// conexiones entrantes que comparten esta pantalla y controlan este equipo.
    pub fn start_host(self: &Arc<Self>, app: AppHandle, id: String, capture: Arc<CaptureEngine>) {
        *self.self_id.lock().unwrap() = id.clone();

        // 1) Responder de descubrimiento (UDP broadcast).
        {
            let id = id.clone();
            std::thread::Builder::new()
                .name("otis-discovery".into())
                .spawn(move || discovery_responder(id))
                .ok();
        }

        // 2) Listeners TCP de sesiones entrantes (misma red local): uno para
        //    video, otro para entrada.
        {
            let this = self.clone();
            let app = app.clone();
            let capture = capture.clone();
            std::thread::Builder::new()
                .name("otis-host".into())
                .spawn(move || this.host_listener(app, capture))
                .ok();
        }

        // 3) Registro en el relay para conexiones FUERA de la red local.
        {
            let this = self.clone();
            std::thread::Builder::new()
                .name("otis-relay-host".into())
                .spawn(move || this.relay_host_loop(app, id, capture))
                .ok();
        }
    }

    /// Mantiene el equipo registrado en el relay y atiende una sesion entrante
    /// por internet cada vez que un visor se conecta a traves de el. El video
    /// y la entrada son dos tuneles independientes (mismo ID + sufijo).
    fn relay_host_loop(self: Arc<Self>, app: AppHandle, id: String, capture: Arc<CaptureEngine>) {
        let video_id = format!("{id}0");
        let input_id = format!("{id}1");
        loop {
            let app_reg = app.clone();
            let vid = video_id.clone();
            match crate::relay::host_tunnel(&video_id, move || {
                let _ = app_reg.emit(
                    "relay-host-status",
                    serde_json::json!({ "ok": true, "id": vid }),
                );
            }) {
                Ok(video_stream) => {
                    if self.incoming_active.swap(true, Ordering::SeqCst) {
                        let _ = video_stream.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                    match crate::relay::host_tunnel_timeout(&input_id, Duration::from_secs(8)) {
                        Ok(input_stream) => {
                            let _ = app.emit("incoming-session", "internet".to_string());
                            eprintln!("[relay-host] sesion entrante por internet");
                            self.handle_incoming(video_stream, input_stream, &app, &capture);
                        }
                        Err(e) => {
                            eprintln!("[relay-host] el canal de entrada no llego: {e}");
                            let _ = video_stream.shutdown(std::net::Shutdown::Both);
                        }
                    }
                    self.incoming_active.store(false, Ordering::SeqCst);
                    let _ = app.emit("incoming-session-ended", ());
                }
                Err(e) => {
                    // Relay no disponible o conexion caida: reintenta con calma.
                    eprintln!("[relay-host] {e}");
                    let _ = app.emit(
                        "relay-host-status",
                        serde_json::json!({ "ok": false, "msg": e.to_string() }),
                    );
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        }
    }

    fn host_listener(self: Arc<Self>, app: AppHandle, capture: Arc<CaptureEngine>) {
        let video_listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, SESSION_PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[host] no se pudo escuchar en {SESSION_PORT}: {e}");
                return;
            }
        };
        let input_listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, INPUT_PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[host] no se pudo escuchar en {INPUT_PORT}: {e}");
                return;
            }
        };
        input_listener.set_nonblocking(true).ok();

        for incoming in video_listener.incoming() {
            let video_stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let _ = video_stream.set_nodelay(true);

            // Solo una sesion entrante a la vez (MVP).
            if self.incoming_active.swap(true, Ordering::SeqCst) {
                let _ = video_stream.shutdown(std::net::Shutdown::Both);
                continue;
            }

            // El visor abre el canal de video y, justo despues, el de entrada
            // (ver `connect`): lo esperamos con un plazo corto.
            let input_stream = match accept_with_timeout(&input_listener, Duration::from_secs(5)) {
                Some(s) => s,
                None => {
                    eprintln!("[host] el visor no abrio el canal de entrada a tiempo");
                    let _ = video_stream.shutdown(std::net::Shutdown::Both);
                    self.incoming_active.store(false, Ordering::SeqCst);
                    continue;
                }
            };
            let _ = input_stream.set_nodelay(true);

            let peer = input_stream
                .peer_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_default();
            let _ = app.emit("incoming-session", peer.clone());
            eprintln!("[host] sesion entrante desde {peer}");

            self.handle_incoming(video_stream, input_stream, &app, &capture);

            self.incoming_active.store(false, Ordering::SeqCst);
            let _ = app.emit("incoming-session-ended", ());
        }
    }

    /// Atiende una conexion entrante: arranca la captura con un sink que envia
    /// frames por el socket de video, y lee eventos de entrada del socket de
    /// entrada para inyectarlos (o pedir una keyframe si el visor la necesita).
    fn handle_incoming(
        &self,
        video_stream: TcpStream,
        mut input_stream: TcpStream,
        app: &AppHandle,
        capture: &Arc<CaptureEngine>,
    ) {
        // Acota el retraso del canal de video (LAN directo o loopback del relay):
        // buffers pequenos => no se apilan frames en vuelo.
        tune_video_socket(&video_stream, true, true);

        // Lee el perfil pedido por el visor (primer mensaje del canal de entrada).
        let profile = negotiate_profile(&mut input_stream);

        let mut input_write = match input_stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };

        // Pide autorizacion al usuario de este equipo antes de compartir nada.
        // Si no responde en 20s, se rechaza sola (evita quedar esperando para
        // siempre a un usuario que no esta presente).
        let peer = input_stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        *self.pending_decision.lock().unwrap() = Some(tx);
        let _ = app.emit(
            "incoming-request",
            serde_json::json!({ "peer": peer, "profile": profile }),
        );
        let accepted = rx.recv_timeout(APPROVAL_TIMEOUT).unwrap_or(false);
        *self.pending_decision.lock().unwrap() = None;
        let _ = app.emit("incoming-request-resolved", serde_json::json!({ "accepted": accepted }));
        if !accepted {
            let _ = write_msg(&mut input_write, MSG_INPUT, br#"{"t":"rejected"}"#);
            return;
        }

        let video_kill = match video_stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        let input_kill = match input_stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        *self.incoming_kill.lock().unwrap() = Some((video_kill, input_kill));
        let _ = app.emit("incoming-session-started", serde_json::json!({ "peer": peer }));

        let alive = Arc::new(AtomicBool::new(true));
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let sink = make_video_sink(video_stream, &profile, alive.clone(), force_keyframe.clone());

        // Reinicia la captura y la arranca alimentando el sink.
        capture.stop();
        let handle = app.clone();
        capture.start(
            move |stats: CaptureStats| {
                let _ = handle.emit("capture-stats", stats);
            },
            Some(sink),
        );

        // Bucle de lectura de entrada (bloquea hasta que el visor corta).
        loop {
            if !alive.load(Ordering::SeqCst) {
                break;
            }
            match read_msg(&mut input_stream) {
                Ok((MSG_INPUT, payload)) => {
                    if is_keyframe_request(&payload) {
                        force_keyframe.store(true, Ordering::SeqCst);
                    } else {
                        apply_input(&payload);
                    }
                }
                Ok(_) => {} // ignora tipos desconocidos
                Err(_) => break, // conexion cerrada
            }
        }

        alive.store(false, Ordering::SeqCst);
        capture.stop();
        *self.incoming_kill.lock().unwrap() = None;
    }

    // ---- Lado visor --------------------------------------------------------

    /// Conecta a un peer por ID (descubrimiento LAN) o por IP directa, y empieza
    /// a recibir su pantalla. Emite `remote-frame`/`remote-frame-h264` y
    /// `remote-metrics` a la UI. Abre DOS conexiones: video y entrada.
    pub fn connect(&self, app: AppHandle, peer_id: &str, profile: &str) -> Result<(), String> {
        // Primero se intenta la red local (rapido, sin servidor); si el equipo no
        // esta en la LAN, se conecta por el relay (fuera de la red local).
        let (video_stream, input_stream) = match resolve_peer(peer_id) {
            Some(video_addr) => {
                let video = TcpStream::connect_timeout(&video_addr, Duration::from_secs(5))
                    .map_err(|e| format!("No se pudo conectar: {e}"))?;
                let input_addr = SocketAddr::new(video_addr.ip(), INPUT_PORT);
                let input = TcpStream::connect_timeout(&input_addr, Duration::from_secs(5))
                    .map_err(|e| format!("No se pudo conectar (entrada): {e}"))?;
                (video, input)
            }
            None => {
                let id: String = peer_id.chars().filter(|c| c.is_ascii_digit()).collect();
                if id.len() < 6 {
                    return Err("Introduce un ID válido de 9 dígitos.".to_string());
                }
                let video = crate::relay::viewer_tunnel(&format!("{id}0"))?;
                let input = crate::relay::viewer_tunnel(&format!("{id}1"))?;
                (video, input)
            }
        };
        video_stream.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
        input_stream.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
        // Buffer de recepcion de video pequeno: si el visor se atrasa pintando,
        // el buffer se llena, TCP frena al host y este descarta frames viejos en
        // vez de que nos llegue video con segundos de retraso.
        tune_video_socket(&video_stream, false, true);

        let mut input_write = input_stream.try_clone().map_err(|e| format!("clone: {e}"))?;
        let hello = serde_json::json!({ "t": "hello", "profile": profile }).to_string();
        write_msg(&mut input_write, MSG_INPUT, hello.as_bytes()).map_err(|e| format!("hello: {e}"))?;

        let running = Arc::new(AtomicBool::new(true));

        let video_kill = video_stream.try_clone().map_err(|e| format!("clone: {e}"))?;
        let input_kill_for_video = input_write.try_clone().map_err(|e| format!("clone: {e}"))?;
        let video_kill_for_input = video_kill.try_clone().map_err(|e| format!("clone: {e}"))?;

        // Guardar handles de escritura/cierre para reenviar entrada y desconectar.
        *self.viewer.lock().unwrap() = Some(ViewerHandle {
            input_write,
            video_kill,
            running: running.clone(),
        });

        // Hilo lector de video.
        {
            let app = app.clone();
            let running = running.clone();
            std::thread::Builder::new()
                .name("otis-viewer-video".into())
                .spawn(move || viewer_video_reader(app, video_stream, running, input_kill_for_video))
                .ok();
        }
        // Hilo lector de entrada/control (mensajes que el host manda de vuelta,
        // como "rejected").
        std::thread::Builder::new()
            .name("otis-viewer-input".into())
            .spawn(move || viewer_input_reader(app, input_stream, running, video_kill_for_input))
            .ok();

        Ok(())
    }

    /// Reenvia un evento de entrada al host (si hay sesion de visor activa).
    pub fn send_input(&self, ev: &Value) {
        let mut guard = self.viewer.lock().unwrap();
        if let Some(v) = guard.as_mut() {
            let json = ev.to_string();
            if write_msg(&mut v.input_write, MSG_INPUT, json.as_bytes()).is_err() {
                v.running.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Pide al host una keyframe (p. ej. tras artefactos visibles de perdida
    /// de paquetes en la conexion por internet).
    pub fn request_keyframe(&self) {
        let mut guard = self.viewer.lock().unwrap();
        if let Some(v) = guard.as_mut() {
            let _ = write_msg(&mut v.input_write, MSG_INPUT, br#"{"t":"keyframe_request"}"#);
        }
    }

    /// Cierra la sesion de visor.
    pub fn disconnect(&self) {
        if let Some(v) = self.viewer.lock().unwrap().take() {
            v.running.store(false, Ordering::SeqCst);
            let _ = v.input_write.shutdown(std::net::Shutdown::Both);
            let _ = v.video_kill.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
    }
}

/// Acepta en `listener` (debe estar en modo no bloqueante) esperando hasta
/// `timeout`. `None` si no llego nadie a tiempo.
fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((s, _)) => return Some(s),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Lee el hello inicial del visor y devuelve el perfil pedido (o ultraligero).
fn negotiate_profile(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .ok();
    let profile = match read_msg(stream) {
        Ok((MSG_INPUT, payload)) => serde_json::from_slice::<Value>(&payload)
            .ok()
            .and_then(|v| v.get("profile").and_then(|p| p.as_str().map(String::from)))
            .unwrap_or_else(|| "ultralight".into()),
        _ => "ultralight".into(),
    };
    // Restaurar modo bloqueante para el resto de la sesion.
    stream.set_read_timeout(None).ok();
    profile
}

fn is_keyframe_request(payload: &[u8]) -> bool {
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|v| v.get("t").and_then(|t| t.as_str()).map(|s| s == "keyframe_request"))
        .unwrap_or(false)
}

/// Aplica un evento de entrada recibido (JSON) inyectandolo localmente.
fn apply_input(payload: &[u8]) {
    let ev: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return,
    };
    let t = ev.get("t").and_then(|v| v.as_str()).unwrap_or("");
    match t {
        "move" => {
            let x = ev.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let y = ev.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
            input::move_mouse(x, y);
        }
        "btn" => {
            let button = ev.get("button").and_then(|v| v.as_str()).unwrap_or("left");
            let down = ev.get("down").and_then(|v| v.as_bool()).unwrap_or(false);
            let _ = input::mouse_button(button, down);
        }
        "scroll" => {
            let delta = ev.get("delta").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            input::scroll(delta);
        }
        "key" => {
            let vk = ev.get("vk").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let code = ev.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let down = ev.get("down").and_then(|v| v.as_bool()).unwrap_or(false);
            input::key(vk, code, down);
        }
        "text" => {
            if let Some(s) = ev.get("text").and_then(|v| v.as_str()) {
                input::type_text(s);
            }
        }
        _ => {}
    }
}

/// Hilo lector del canal de VIDEO del visor: recibe frames y los emite a la UI
/// (JPEG base64 o H.264 Annex B base64 segun el codec del payload). Si el
/// canal se cierra, tambien cierra el de entrada para que su hilo no se quede
/// bloqueado leyendo para siempre.
fn viewer_video_reader(app: AppHandle, mut stream: TcpStream, running: Arc<AtomicBool>, input_kill: TcpStream) {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut frames = 0u32;
    let mut bytes = 0u64;
    let mut window = Instant::now();
    // Coalescido de emision hacia el WebView: como mucho un frame cada ~14 ms.
    // Si llegan varios de golpe (rafaga tras un bache de red) se pinta solo el
    // ultimo -> no se acumula retraso en la cola de eventos. Solo aplica a JPEG
    // (cada frame es independiente); H.264 nunca se salta (dependencia de keyframe).
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    const EMIT_MIN_GAP: Duration = Duration::from_millis(14);

    while running.load(Ordering::SeqCst) {
        match read_msg(&mut stream) {
            Ok((MSG_FRAME, payload)) => {
                if payload.len() < 10 {
                    continue;
                }
                let codec = payload[0];
                let keyframe = payload[1] != 0;
                let w = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
                let h = u32::from_be_bytes([payload[6], payload[7], payload[8], payload[9]]);
                let data = &payload[10..];
                frames += 1;
                bytes += data.len() as u64;

                // Solo se puede COALESCAR (saltar) un frame JPEG completo: es
                // autonomo. H.264 y JPEG-por-celdas son incrementales -> siempre
                // se emiten o el visor se queda con trozos viejos.
                let coalescable = codec == CODEC_JPEG;
                if !coalescable || last_emit.elapsed() >= EMIT_MIN_GAP {
                    let encoded = b64.encode(data);
                    match codec {
                        CODEC_H264 => {
                            let _ = app.emit(
                                "remote-frame-h264",
                                serde_json::json!({ "data": encoded, "width": w, "height": h, "keyframe": keyframe }),
                            );
                        }
                        CODEC_JPEG_TILES => {
                            let _ = app.emit(
                                "remote-frame-tiles",
                                serde_json::json!({ "data": encoded, "width": w, "height": h, "keyframe": keyframe }),
                            );
                        }
                        _ => {
                            let _ = app.emit(
                                "remote-frame",
                                serde_json::json!({ "jpeg": encoded, "width": w, "height": h }),
                            );
                        }
                    }
                    last_emit = Instant::now();
                }

                if window.elapsed() >= Duration::from_millis(500) {
                    let secs = window.elapsed().as_secs_f32().max(0.001);
                    let _ = app.emit(
                        "remote-metrics",
                        serde_json::json!({
                            "fps": frames as f32 / secs,
                            "kbps": (bytes as f32 * 8.0 / 1000.0) / secs,
                            "latency_ms": 0.0,
                        }),
                    );
                    frames = 0;
                    bytes = 0;
                    window = Instant::now();
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    running.store(false, Ordering::SeqCst);
    let _ = input_kill.shutdown(std::net::Shutdown::Both);
    let _ = app.emit("remote-ended", serde_json::json!({ "reason": "closed" }));
}

/// Hilo lector del canal de ENTRADA/control del visor: normalmente solo recibe
/// el "rejected" si el host no autoriza la sesion. Si se cierra, tambien cierra
/// el de video.
fn viewer_input_reader(app: AppHandle, mut stream: TcpStream, running: Arc<AtomicBool>, video_kill: TcpStream) {
    while running.load(Ordering::SeqCst) {
        match read_msg(&mut stream) {
            Ok((MSG_INPUT, payload)) => {
                if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                    if v.get("t").and_then(|t| t.as_str()) == Some("rejected") {
                        running.store(false, Ordering::SeqCst);
                        let _ = video_kill.shutdown(std::net::Shutdown::Both);
                        let _ = app.emit("remote-ended", serde_json::json!({ "reason": "rejected" }));
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = video_kill.shutdown(std::net::Shutdown::Both);
}

// ===========================================================================
// Descubrimiento LAN por ID (UDP broadcast).
// ===========================================================================

/// Responde a "OTIS?<id>" con "OTIS!<id>|<puerto>" si el ID es el nuestro.
fn discovery_responder(id: String) {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[discovery] no se pudo enlazar UDP {DISCOVERY_PORT}: {e}");
            return;
        }
    };
    let _ = socket.set_broadcast(true);
    let mut buf = [0u8; 256];
    let want = format!("OTIS?{id}");
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => {
                if let Ok(msg) = std::str::from_utf8(&buf[..n]) {
                    if msg.trim() == want {
                        let reply = format!("OTIS!{id}|{SESSION_PORT}");
                        let _ = socket.send_to(reply.as_bytes(), src);
                    }
                }
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Comprueba si un ID responde al descubrimiento LAN ahora mismo (para el
/// punto de estado de la libreta de dispositivos). No abre sesion.
pub fn is_online_lan(peer_id: &str) -> bool {
    resolve_peer(peer_id).is_some()
}

/// Resuelve un peer: si parece IP, conecta directo; si es un ID, lo busca por
/// broadcast en la LAN. Devuelve la direccion del socket de VIDEO (el de
/// entrada esta en `SESSION_PORT + 1`, ver `INPUT_PORT`, en la misma IP).
fn resolve_peer(peer_id: &str) -> Option<SocketAddr> {
    let trimmed = peer_id.trim();
    // IP directa (util para pruebas).
    if let Ok(ip) = trimmed.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, SESSION_PORT));
    }

    let id: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if id.is_empty() {
        return None;
    }

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_broadcast(true).ok()?;
    socket
        .set_read_timeout(Some(Duration::from_millis(450)))
        .ok()?;
    let query = format!("OTIS?{id}");
    let bcast = SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT));

    let mut buf = [0u8; 256];
    // Varios intentos (los broadcast se pierden a veces). Pocos y cortos: si el
    // equipo no esta en la LAN, no queremos que el usuario espere 4 s antes de
    // que la conexion pase al relay por internet.
    for _ in 0..3 {
        if socket.send_to(query.as_bytes(), bcast).is_err() {
            continue;
        }
        if let Ok((n, src)) = socket.recv_from(&mut buf) {
            if let Ok(msg) = std::str::from_utf8(&buf[..n]) {
                if let Some(rest) = msg.trim().strip_prefix(&format!("OTIS!{id}|")) {
                    if let Ok(port) = rest.parse::<u16>() {
                        return Some(SocketAddr::new(src.ip(), port));
                    }
                }
            }
        }
    }
    None
}
