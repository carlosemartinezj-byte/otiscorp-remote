//! Cliente del relay/rendezvous para conexiones FUERA de la red local.
//!
//! Cuando los dos equipos no comparten red, no pueden verse por broadcast ni
//! conectarse directo (cada uno detras de su router/NAT). La solucion: ambos
//! abren una conexion SALIENTE al relay (eso atraviesa cualquier NAT/firewall
//! sin configurar nada) y el relay los empareja por el ID de 9 digitos.
//!
//! Este modulo habla TLS con el relay (Fly.io termina TLS en el borde, asi que
//! el trafico por internet va cifrado) y hace el handshake `HOST`/`JOIN`. Para
//! no tener que reescribir todo el codigo de sesion (que usa `TcpStream`), una
//! vez emparejado **puentea** el tunel TLS a un socket loopback local y devuelve
//! el `TcpStream` de ese socket: el resto del transporte lo usa igual que una
//! conexion directa a un peer.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// Host y puerto del relay. Se puede sobreescribir con la variable de entorno
/// OTISCORP_RELAY (formato "host:puerto") para pruebas.
const DEFAULT_RELAY: &str = "otiscorp-relay.fly.dev:443";

fn relay_target() -> (String, u16) {
    let raw = std::env::var("OTISCORP_RELAY").unwrap_or_else(|_| DEFAULT_RELAY.to_string());
    match raw.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(443)),
        None => (raw, 443),
    }
}

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// Buffers de socket pequenos en el puente del relay: igual que en `transport`,
/// evita que se apilen frames de video en vuelo (retraso de segundos).
///
/// Ademas KEEPALIVE TCP: si la conexion al relay se queda "medio muerta" (Fly
/// reinicia, corte de red), el host esta bloqueado en `host_tunnel` esperando
/// "PEER" para siempre y el relay ya no lo tiene registrado -> el visor recibe
/// NOHOST. Con keepalive el SO sondea cada pocos segundos y la lectura falla en
/// ~30 s, asi `relay_host_loop` reconecta y se vuelve a registrar solo.
fn tune_relay_socket(stream: &TcpStream) {
    let s = socket2::SockRef::from(stream);
    let _ = s.set_nodelay(true);
    let _ = s.set_send_buffer_size(96 * 1024);
    let _ = s.set_recv_buffer_size(96 * 1024);
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5));
    let _ = s.set_tcp_keepalive(&ka);
}

/// Construye la config TLS con las raices publicas (webpki-roots) y el proveedor
/// criptografico `ring`.
fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versiones TLS")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Abre la conexion TLS al relay y envia la linea de handshake `"{cmd} {id}\n"`.
fn tls_connect(cmd: &str, id: &str) -> io::Result<Tls> {
    let (host, port) = relay_target();
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nombre de servidor invalido"))?
        .to_owned();
    let conn = rustls::ClientConnection::new(tls_config(), server_name)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("TLS: {e}")))?;

    let sock = TcpStream::connect((host.as_str(), port))?;
    tune_relay_socket(&sock);
    let mut tls = rustls::StreamOwned::new(conn, sock);
    tls.write_all(format!("{cmd} {id}\n").as_bytes())?;
    tls.flush()?;
    Ok(tls)
}

/// Lee bytes hasta encontrar '\n'. Devuelve (linea_sin_\n, sobrante_tras_\n).
fn read_line(tls: &mut Tls) -> io::Result<(String, Vec<u8>)> {
    let mut acc: Vec<u8> = Vec::with_capacity(32);
    let mut byte = [0u8; 1];
    loop {
        let n = tls.read(&mut byte)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "relay cerro"));
        }
        if byte[0] == b'\n' {
            break;
        }
        acc.push(byte[0]);
        if acc.len() > 64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "linea demasiado larga"));
        }
    }
    Ok((String::from_utf8_lossy(&acc).trim().to_string(), Vec::new()))
}

/// Lado HOST: registra este equipo en el relay y ESPERA (bloquea) hasta que un
/// visor se conecte. Cuando llega, devuelve un `TcpStream` (loopback) que actua
/// como la conexion de sesion entrante, equivalente a un `accept()` directo.
pub fn host_tunnel(id: &str) -> io::Result<TcpStream> {
    let mut tls = tls_connect("HOST", id)?;
    // Espera la senal "PEER" del relay (llega cuando un visor hace JOIN).
    let (line, _) = read_line(&mut tls)?;
    if line != "PEER" {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("respuesta inesperada del relay: {line}"),
        ));
    }
    bridge(tls)
}

/// Igual que `host_tunnel`, pero con un plazo maximo de espera por el visor.
/// Se usa para el canal de ENTRADA de una sesion por internet: el canal de
/// video ya emparejo a los dos equipos, asi que si el visor no abre tambien
/// el canal de entrada en `timeout`, algo fue mal y abortamos esa sesion en
/// vez de quedarnos esperando para siempre.
pub fn host_tunnel_timeout(id: &str, timeout: Duration) -> io::Result<TcpStream> {
    let mut tls = tls_connect("HOST", id)?;
    tls.get_ref().set_read_timeout(Some(timeout)).ok();
    let (line, _) = read_line(&mut tls)?;
    if line != "PEER" {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("respuesta inesperada del relay: {line}"),
        ));
    }
    tls.get_ref().set_read_timeout(None).ok();
    bridge(tls)
}

/// Lado VISOR: pide al relay conectar con `id`. Devuelve un `TcpStream`
/// (loopback) equivalente a haber conectado directo al host.
pub fn viewer_tunnel(id: &str) -> Result<TcpStream, String> {
    let mut tls = tls_connect("JOIN", id).map_err(|e| format!("No se pudo contactar el servidor: {e}"))?;
    let (line, _) = read_line(&mut tls).map_err(|e| format!("relay: {e}"))?;
    match line.as_str() {
        "OK" => bridge(tls).map_err(|e| format!("puente: {e}")),
        "NOHOST" => Err("El equipo no está conectado al servidor (¿está encendido y con OtisCorp abierto?)".into()),
        other => Err(format!("respuesta del relay: {other}")),
    }
}

/// Puentea el tunel TLS a un socket loopback y devuelve el extremo del llamante.
/// Un hilo copia bytes en ambos sentidos entre el TLS (internet) y el loopback
/// (que consume el codigo de sesion como si fuera una conexion directa).
fn bridge(tls: Tls) -> io::Result<TcpStream> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    let outer = TcpStream::connect(addr)?; // extremo que se devuelve al llamante
    tune_relay_socket(&outer);
    let (inner, _) = listener.accept()?; // extremo del puente
    tune_relay_socket(&inner);

    std::thread::Builder::new()
        .name("otis-relay-bridge".into())
        .spawn(move || pump(tls, inner))
        .ok();

    Ok(outer)
}

/// Bombea bytes full-duplex entre el TLS y el socket interno, en un solo hilo,
/// usando lecturas con timeout corto (las escrituras quedan bloqueantes).
///
/// Buffer grande (128 KB) y VACIADO por rafagas: en cada pasada mueve hasta 8
/// bloques seguidos por sentido antes de cambiar. Asi un frame JPEG entero cruza
/// en una sola pasada en vez de en ~13 idas y vueltas de 32 KB con `flush` entre
/// medias (lo que limitaba mucho el throughput por el relay).
fn pump(mut tls: Tls, mut inner: TcpStream) {
    let to = Duration::from_millis(5);
    tls.get_ref().set_read_timeout(Some(to)).ok();
    inner.set_read_timeout(Some(to)).ok();

    let mut buf = [0u8; 128 * 1024];
    'pump: loop {
        let mut idle = true;

        // inner (sesion local) -> tls (internet)
        for _ in 0..8 {
            match inner.read(&mut buf) {
                Ok(0) => break 'pump, // sesion local cerrada
                Ok(n) => {
                    idle = false;
                    if tls.write_all(&buf[..n]).is_err() {
                        break 'pump;
                    }
                }
                Err(e) if is_would_block(&e) => break,
                Err(_) => break 'pump,
            }
        }
        if !idle {
            let _ = tls.flush();
        }

        // tls (internet) -> inner (sesion local)
        for _ in 0..8 {
            match tls.read(&mut buf) {
                Ok(0) => break 'pump, // relay/peer cerro
                Ok(n) => {
                    idle = false;
                    if inner.write_all(&buf[..n]).is_err() {
                        break 'pump;
                    }
                }
                Err(e) if is_would_block(&e) => break,
                Err(_) => break 'pump,
            }
        }

        if idle {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    let _ = inner.shutdown(std::net::Shutdown::Both);
    let _ = tls.get_ref().shutdown(std::net::Shutdown::Both);
}

fn is_would_block(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
