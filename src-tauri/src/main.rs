// OtisCorp Remote — cliente de escritorio remoto ligero.
// Arranca directo a la pantalla principal: sin codigo de activacion, con un ID
// propio auto-generado y persistente, y acceso desatendido activo por defecto.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod identity;
mod input;
mod netscan;
mod sysprofile;
mod transport;

use capture::{CaptureEngine, CaptureStats};
use identity::Identity;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use sysprofile::{ClientMetrics, Monitor, SystemProfile};
use tauri::{Emitter, Manager, State};
use transport::Transport;

/// Estado global de la app.
struct AppState {
    app_data_dir: PathBuf,
    identity: Mutex<Identity>,
    session_password: Mutex<String>,
    monitor: Monitor,
    capture: Arc<CaptureEngine>,
    transport: Arc<Transport>,
}

/// Carga util que consume la pantalla principal al arrancar.
#[derive(Serialize)]
struct Bootstrap {
    id: String,
    device_name: String,
    unattended: bool,
    session_password: String,
    profile: SystemProfile,
}

#[tauri::command]
fn bootstrap(state: State<AppState>) -> Bootstrap {
    let identity = state.identity.lock().unwrap().clone();
    let session_password = state.session_password.lock().unwrap().clone();
    Bootstrap {
        id: identity.id,
        device_name: identity.device_name,
        unattended: identity.unattended,
        session_password,
        profile: state.monitor.profile(),
    }
}

/// Metricas en vivo del cliente (RAM MB, CPU %). Polling desde la UI.
#[tauri::command]
fn client_metrics(state: State<AppState>) -> ClientMetrics {
    state.monitor.client_metrics()
}

/// Regenera la contrasena de sesion de 4 digitos.
#[tauri::command]
fn regenerate_password(state: State<AppState>) -> String {
    let pw = identity::generate_session_password();
    *state.session_password.lock().unwrap() = pw.clone();
    pw
}

/// Arranca el motor de captura de pantalla (DXGI Desktop Duplication).
/// Emite el evento `capture-stats` ~2 veces/seg con fps/resolucion/throughput.
#[tauri::command]
fn start_capture(app: tauri::AppHandle, state: State<AppState>) -> Result<(), String> {
    let handle = app.clone();
    state.capture.start(
        move |stats: CaptureStats| {
            let _ = handle.emit("capture-stats", stats);
        },
        None,
    );
    Ok(())
}

/// Detiene el motor de captura.
#[tauri::command]
fn stop_capture(state: State<AppState>) {
    state.capture.stop();
}

/// Estado actual del motor de captura (para polling puntual desde la UI).
#[tauri::command]
fn capture_status(state: State<AppState>) -> CaptureStats {
    state.capture.stats()
}

// ---- Inyeccion de entrada remota (control de raton/teclado) ---------------
// Los llama el lado visor a traves del transporte. Coordenadas normalizadas 0..1.
#[tauri::command]
fn input_mouse_move(x: f64, y: f64) {
    input::move_mouse(x, y);
}

#[tauri::command]
fn input_mouse_button(button: String, down: bool) -> Result<(), String> {
    input::mouse_button(&button, down)
}

#[tauri::command]
fn input_scroll(delta: i32) {
    input::scroll(delta);
}

#[tauri::command]
fn input_key(vk: u16, code: String, down: bool) {
    input::key(vk, &code, down);
}

#[tauri::command]
fn input_text(text: String) {
    input::type_text(&text);
}

// ---- Sesion remota (lado visor) -------------------------------------------
/// Conecta a un peer por ID (descubrimiento LAN) y empieza a recibir su pantalla.
#[tauri::command]
fn connect_peer(
    app: tauri::AppHandle,
    state: State<AppState>,
    peer_id: String,
    profile: String,
) -> Result<(), String> {
    state.transport.connect(app, &peer_id, &profile)
}

/// Cierra la sesion de visor en curso.
#[tauri::command]
fn disconnect_peer(state: State<AppState>) {
    state.transport.disconnect();
}

/// Reenvia un evento de entrada (raton/teclado) al equipo remoto.
#[tauri::command]
fn send_remote_input(state: State<AppState>, ev: serde_json::Value) {
    state.transport.send_input(&ev);
}

// ---- Modo P2P por internet (WebRTC en el WebView) -------------------------
/// Arranca la captura emitiendo frames al propio WebView (evento `local-frame`),
/// que el frontend reenvia por el data channel de WebRTC al visor.
#[tauri::command]
fn start_sharing(app: tauri::AppHandle, state: State<AppState>, profile: String) {
    let sink = transport::make_local_sink(app.clone(), &profile);
    state.capture.stop();
    let handle = app.clone();
    state.capture.start(
        move |stats: CaptureStats| {
            let _ = handle.emit("capture-stats", stats);
        },
        Some(sink),
    );
}

/// Detiene la captura del modo compartir por WebRTC.
#[tauri::command]
fn stop_sharing(state: State<AppState>) {
    state.capture.stop();
}

/// Renombra el puesto y persiste el cambio.
#[tauri::command]
fn rename_device(state: State<AppState>, name: String) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("El nombre no puede estar vacio".into());
    }
    let mut id = state.identity.lock().unwrap();
    id.device_name = name.to_string();
    identity::save(&state.app_data_dir, &id);
    Ok(())
}

/// Escanea la red local (tabla ARP) y devuelve los dispositivos visibles:
/// IP, MAC, fabricante (por prefijo OUI) y nombre de host si se puede resolver.
/// Es la misma informacion que muestra el panel de cualquier router domestico;
/// no inspecciona ni intercepta trafico de otros equipos.
#[tauri::command]
fn scan_network() -> Vec<netscan::NetDevice> {
    netscan::scan()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from("."));

            let default_name = sysinfo::System::host_name().unwrap_or_else(|| "Mi PC".to_string());
            let identity = identity::load_or_create(&app_data_dir, default_name);
            let session_password = identity::generate_session_password();

            let capture = Arc::new(CaptureEngine::new());
            let transport = Arc::new(Transport::new());

            // Acceso desatendido: arranca el host (descubrimiento + escucha) para
            // que otro equipo de la LAN pueda ver esta pantalla y controlarla.
            transport.start_host(app.handle().clone(), identity.id.clone(), capture.clone());

            app.manage(AppState {
                app_data_dir,
                identity: Mutex::new(identity),
                session_password: Mutex::new(session_password),
                monitor: Monitor::new(),
                capture,
                transport,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            client_metrics,
            regenerate_password,
            rename_device,
            start_capture,
            stop_capture,
            capture_status,
            input_mouse_move,
            input_mouse_button,
            input_scroll,
            input_key,
            input_text,
            connect_peer,
            disconnect_peer,
            send_remote_input,
            start_sharing,
            stop_sharing,
            scan_network
        ])
        .run(tauri::generate_context!())
        .expect("error al arrancar OtisCorp Remote");
}
