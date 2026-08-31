//! Transporte de sesion remota (compartir pantalla + control).
//!
//! MVP para dos equipos en la MISMA red local (el caso "la otra PC de mi
//! empresa"): sin servidor externo. Un equipo hace de **host** (comparte su
//! pantalla y recibe control) y otro de **visor** (ve la pantalla y controla).
//!
//! - Descubrimiento por ID: UDP broadcast en la LAN. El visor pregunta
//!   "quien tiene el ID X" y el host responde con su IP + puerto TCP.
//! - Sesion: TCP con framing por mensajes. El host envia frames MJPEG; el visor
//!   envia eventos de entrada (raton/teclado) que el host inyecta con SendInput.
//! - Codec: MJPEG (jpeg-encoder, Rust puro). La calidad/escala/fps salen del
//!   perfil (ultraligero/equilibrado/nitido) para ajustarse a equipos lentos.
//!
//! Pendiente (fuera del MVP): cifrado TLS 1.3, dialogo de aprobacion entrante
//! con auto-rechazo, y rendezvous para conexiones fuera de la LAN.

use crate::capture::{CaptureEngine, CaptureStats, FrameSink};
use crate::input;
use base64::Engine;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const DISCOVERY_PORT: u16 = 49321;
const SESSION_PORT: u16 = 49322;

const MSG_FRAME: u8 = 0x01;
const MSG_INPUT: u8 = 0x10;

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

// ---- Perfil de calidad -> parametros de codificacion -----------------------

struct Quality {
    jpeg: u8,
    scale: u32,          // divisor de resolucion (1 = nativa)
    min_interval: Duration, // tope de fps
}

fn quality_for(profile: &str) -> Quality {
    match profile {
        "sharp" => Quality { jpeg: 85, scale: 1, min_interval: Duration::from_millis(33) },
        "balanced" => Quality { jpeg: 70, scale: 1, min_interval: Duration::from_millis(33) },
        // ultraligero: media resolucion, 30 fps (max), calidad mejorada.
        _ => Quality { jpeg: 65, scale: 2, min_interval: Duration::from_millis(33) },
    }
}

// ---- Sink del host: codifica cada frame a JPEG y lo envia por TCP ----------

struct JpegSender {
    stream: TcpStream,
    quality: Quality,
    alive: Arc<AtomicBool>,
    last_sent: Instant,
    scaled: Vec<u8>,
    enc_buf: Vec<u8>,
}

impl FrameSink for JpegSender {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8]) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        // Tope de fps segun perfil (descarta frames sobrantes).
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

        // Payload FRAME: [u32 w][u32 h][jpeg...]
        let mut payload = Vec::with_capacity(8 + self.enc_buf.len());
        payload.extend_from_slice(&sw.to_be_bytes());
        payload.extend_from_slice(&sh.to_be_bytes());
        payload.extend_from_slice(&self.enc_buf);

        if write_msg(&mut self.stream, MSG_FRAME, &payload).is_err() {
            self.alive.store(false, Ordering::SeqCst);
        } else {
            self.last_sent = Instant::now();
        }
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

// ---- Sink local: emite cada frame al PROPIO WebView (para reenviar por WebRTC).
// Lo usa el modo P2P por internet: el capturador entrega el frame, aqui se
// codifica a JPEG y se emite el evento `local-frame`; el frontend lo manda por
// el data channel de WebRTC al visor.

struct LocalFrameSink {
    app: AppHandle,
    quality: Quality,
    last_sent: Instant,
    scaled: Vec<u8>,
    enc_buf: Vec<u8>,
}

impl FrameSink for LocalFrameSink {
    fn on_frame(&mut self, width: u32, height: u32, bgra: &[u8]) {
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
pub fn make_local_sink(app: AppHandle, profile: &str) -> Box<dyn FrameSink> {
    Box::new(LocalFrameSink {
        app,
        quality: quality_for(profile),
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
    incoming_kill: Mutex<Option<TcpStream>>,
}

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(20);

struct ViewerHandle {
    stream: TcpStream, // handle de escritura (clonado) para reenviar entrada
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

    /// Corta la sesion entrante activa (LAN) desde ESTE equipo (el que esta
    /// siendo controlado). Cierra el socket, lo que desbloquea la lectura y
    /// termina el bucle de `handle_incoming` de forma normal.
    pub fn end_incoming(&self) {
        if let Some(s) = self.incoming_kill.lock().unwrap().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
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

        // 2) Listener TCP de sesiones entrantes.
        {
            let this = self.clone();
            std::thread::Builder::new()
                .name("otis-host".into())
                .spawn(move || this.host_listener(app, capture))
                .ok();
        }
    }

    fn host_listener(self: Arc<Self>, app: AppHandle, capture: Arc<CaptureEngine>) {
        let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, SESSION_PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[host] no se pudo escuchar en {SESSION_PORT}: {e}");
                return;
            }
        };
        for incoming in listener.incoming() {
            let stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Solo una sesion entrante a la vez (MVP).
            if self.incoming_active.swap(true, Ordering::SeqCst) {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                continue;
            }
            let peer = stream
                .peer_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_default();
            let _ = app.emit("incoming-session", peer.clone());
            eprintln!("[host] sesion entrante desde {peer}");

            self.handle_incoming(stream, &app, &capture);

            self.incoming_active.store(false, Ordering::SeqCst);
            let _ = app.emit("incoming-session-ended", ());
        }
    }

    /// Atiende una conexion entrante: arranca la captura con un sink que envia
    /// frames por el socket, y lee eventos de entrada para inyectarlos.
    fn handle_incoming(&self, mut read_stream: TcpStream, app: &AppHandle, capture: &Arc<CaptureEngine>) {
        // Lee el perfil pedido por el visor (primer mensaje INPUT con {t:"hello"}).
        let profile = negotiate_profile(&mut read_stream);

        let mut write_stream = match read_stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };

        // Pide autorizacion al usuario de este equipo antes de compartir nada.
        // Si no responde en 20s, se rechaza sola (evita quedar esperando para
        // siempre a un usuario que no esta presente).
        let peer = read_stream
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
            let _ = write_msg(&mut write_stream, MSG_INPUT, br#"{"t":"rejected"}"#);
            return;
        }
        if let Ok(k) = read_stream.try_clone() {
            *self.incoming_kill.lock().unwrap() = Some(k);
        }
        let _ = app.emit("incoming-session-started", serde_json::json!({ "peer": peer }));

        let alive = Arc::new(AtomicBool::new(true));
        let sink = JpegSender {
            stream: write_stream,
            quality: quality_for(&profile),
            alive: alive.clone(),
            last_sent: Instant::now() - Duration::from_secs(1),
            scaled: Vec::new(),
            enc_buf: Vec::new(),
        };

        // Reinicia la captura y la arranca alimentando el sink.
        capture.stop();
        let handle = app.clone();
        capture.start(
            move |stats: CaptureStats| {
                let _ = handle.emit("capture-stats", stats);
            },
            Some(Box::new(sink)),
        );

        // Bucle de lectura de entrada (bloquea hasta que el visor corta).
        loop {
            if !alive.load(Ordering::SeqCst) {
                break;
            }
            match read_msg(&mut read_stream) {
                Ok((MSG_INPUT, payload)) => apply_input(&payload),
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
    /// a recibir su pantalla. Emite `remote-frame` y `remote-metrics` a la UI.
    pub fn connect(&self, app: AppHandle, peer_id: &str, profile: &str) -> Result<(), String> {
        let addr = resolve_peer(peer_id).ok_or_else(|| {
            "No se encontró el equipo en la red local. ¿Está encendido y con OtisCorp abierto?"
                .to_string()
        })?;

        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .map_err(|e| format!("No se pudo conectar: {e}"))?;
        stream
            .set_nodelay(true)
            .map_err(|e| format!("nodelay: {e}"))?;

        // Enviar hello con el perfil deseado.
        let hello = serde_json::json!({ "t": "hello", "profile": profile }).to_string();
        write_msg(&mut stream, MSG_INPUT, hello.as_bytes()).map_err(|e| format!("hello: {e}"))?;

        let read_stream = stream.try_clone().map_err(|e| format!("clone: {e}"))?;
        let running = Arc::new(AtomicBool::new(true));

        // Guardar handle de escritura para reenviar entrada.
        *self.viewer.lock().unwrap() = Some(ViewerHandle {
            stream,
            running: running.clone(),
        });

        // Hilo lector de frames.
        std::thread::Builder::new()
            .name("otis-viewer".into())
            .spawn(move || viewer_reader(app, read_stream, running))
            .ok();

        Ok(())
    }

    /// Reenvia un evento de entrada al host (si hay sesion de visor activa).
    pub fn send_input(&self, ev: &Value) {
        let mut guard = self.viewer.lock().unwrap();
        if let Some(v) = guard.as_mut() {
            let json = ev.to_string();
            if write_msg(&mut v.stream, MSG_INPUT, json.as_bytes()).is_err() {
                v.running.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Cierra la sesion de visor.
    pub fn disconnect(&self) {
        if let Some(v) = self.viewer.lock().unwrap().take() {
            v.running.store(false, Ordering::SeqCst);
            let _ = v.stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Default for Transport {
    fn default() -> Self {
        Self::new()
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

/// Hilo lector del visor: recibe frames y los emite a la UI como JPEG base64.
fn viewer_reader(app: AppHandle, mut stream: TcpStream, running: Arc<AtomicBool>) {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut frames = 0u32;
    let mut bytes = 0u64;
    let mut window = Instant::now();

    while running.load(Ordering::SeqCst) {
        match read_msg(&mut stream) {
            Ok((MSG_FRAME, payload)) => {
                if payload.len() < 8 {
                    continue;
                }
                let w = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let h = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let jpeg = &payload[8..];
                frames += 1;
                bytes += jpeg.len() as u64;

                let encoded = b64.encode(jpeg);
                let _ = app.emit(
                    "remote-frame",
                    serde_json::json!({ "jpeg": encoded, "width": w, "height": h }),
                );

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
            Ok((MSG_INPUT, payload)) => {
                if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                    if v.get("t").and_then(|t| t.as_str()) == Some("rejected") {
                        running.store(false, Ordering::SeqCst);
                        let _ = app.emit("remote-ended", serde_json::json!({ "reason": "rejected" }));
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    running.store(false, Ordering::SeqCst);
    let _ = app.emit("remote-ended", serde_json::json!({ "reason": "closed" }));
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
/// broadcast en la LAN.
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
        .set_read_timeout(Some(Duration::from_millis(700)))
        .ok()?;
    let query = format!("OTIS?{id}");
    let bcast = SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT));

    let mut buf = [0u8; 256];
    // Varios intentos (los broadcast se pierden a veces).
    for _ in 0..6 {
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
