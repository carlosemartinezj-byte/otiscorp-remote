// OtisCorp Remote — cliente de escritorio remoto ligero.
// Arranca directo a la pantalla principal: sin codigo de activacion, con un ID
// propio auto-generado y persistente, y acceso desatendido activo por defecto.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod h264enc;
mod identity;
mod input;
mod netscan;
mod relay;
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

/// Responde a una solicitud de conexion entrante (LAN): true = autorizar.
#[tauri::command]
fn respond_incoming(state: State<AppState>, accept: bool) {
    state.transport.respond_incoming(accept);
}

/// Corta la sesion entrante activa (LAN) desde el lado que esta siendo
/// controlado (host).
#[tauri::command]
fn end_incoming_session(state: State<AppState>) {
    state.transport.end_incoming();
}

/// Comprueba si un ID responde al descubrimiento LAN ahora mismo (estado
/// en linea/desconectado de la libreta de dispositivos).
#[tauri::command]
fn check_online_lan(peer_id: String) -> bool {
    transport::is_online_lan(&peer_id)
}

/// Reenvia un evento de entrada (raton/teclado) al equipo remoto.
#[tauri::command]
fn send_remote_input(state: State<AppState>, ev: serde_json::Value) {
    state.transport.send_input(&ev);
}

/// Pide al host una keyframe H.264 (recuperacion tras un frame corrupto/perdido
/// en el decoder del visor).
#[tauri::command]
fn request_remote_keyframe(state: State<AppState>) {
    state.transport.request_keyframe();
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

/// Apaga los atajos de teclado que WebView2 se reserva para si mismo (F3
/// buscar, F5 recargar, F6/F10 barra de menu, F11 pantalla completa, F12
/// DevTools, Ctrl+F, Ctrl+P...): los intercepta ANTES de que lleguen al
/// keydown de la pagina, asi que aunque el JS haga preventDefault() nunca
/// los ve para reenviarlos al equipo remoto.
///
/// A proposito NO se llama desde `setup()`: ahi el bucle de eventos de la
/// ventana todavia no arranco, y `with_webview` necesita que ese bucle este
/// vivo para poder despachar el closure al hilo principal -- llamarlo
/// demasiado temprano dejaba el WebView2 colgado sin pintar nunca (se probo
/// y rompia la ventana). El frontend llama este comando por su cuenta, una
/// vez que la pagina ya cargo (ver ui/app.js) -- en ese punto el WebView2
/// esta garantizado vivo porque es literalmente el que esta corriendo el JS
/// que hizo la llamada.
#[tauri::command]
fn disable_browser_accelerator_keys(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(windows)]
    {
        window
            .with_webview(|webview| {
                unsafe {
                    if let Ok(core) = webview.controller().CoreWebView2() {
                        if let Ok(settings) = core.Settings() {
                            // AreBrowserAcceleratorKeysEnabled vive en la
                            // interfaz derivada Settings3, no en la base.
                            use windows_core::Interface as _;
                            if let Ok(settings3) = settings.cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings3>() {
                                let _ = settings3.SetAreBrowserAcceleratorKeysEnabled(false);
                            }
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
    Ok(())
}

/// Estado actual del arranque automatico (para pintar el toggle en Ajustes).
#[tauri::command]
fn autostart_status(app: tauri::AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// Prende/apaga el arranque automatico a mano, desde el toggle de Ajustes.
#[tauri::command]
fn autostart_set(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    if enabled { mgr.enable() } else { mgr.disable() }.map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from("."));

            let default_name = sysinfo::System::host_name().unwrap_or_else(|| "Mi PC".to_string());
            let mut identity = identity::load_or_create(&app_data_dir, default_name);
            let session_password = identity::generate_session_password();

            // Prende el arranque automatico UNA sola vez (primera vez que
            // corre esta version, sea instalacion nueva o actualizacion de
            // una vieja): el acceso desatendido no sirve de nada si hay que
            // abrir la app a mano cada vez que la PC se reinicia sola. Si
            // falla (raro, algun problema de registro) no se marca como
            // hecho, para reintentar en el proximo arranque. Si el usuario
            // lo apaga despues desde Ajustes, el flag ya queda en true y
            // esto nunca mas lo vuelve a tocar.
            if !identity.autostart_initialized {
                use tauri_plugin_autostart::ManagerExt;
                if app.autolaunch().enable().is_ok() {
                    identity.autostart_initialized = true;
                    identity::save(&app_data_dir, &identity);
                }
            }

            let identity = identity;

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
            respond_incoming,
            end_incoming_session,
            check_online_lan,
            send_remote_input,
            request_remote_keyframe,
            start_sharing,
            stop_sharing,
            scan_network,
            disable_browser_accelerator_keys,
            autostart_status,
            autostart_set
        ])
        .run(tauri::generate_context!())
        .expect("error al arrancar OtisCorp Remote");
}
