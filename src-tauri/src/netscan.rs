// OtisCorp Remote — inventario de dispositivos en la red local.
// Lee la tabla ARP del propio equipo (misma info que muestra el panel de
// cualquier router) para listar IP/MAC/fabricante de lo que hay en la LAN.
// No intercepta ni inspecciona trafico de otros dispositivos.

use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::process::Command;

#[derive(Serialize, Clone)]
pub struct NetDevice {
    pub ip: String,
    pub mac: String,
    pub vendor: String,
    pub hostname: String,
}

/// Prefijos OUI (primeros 3 bytes de la MAC) mas comunes en routers/equipos
/// domesticos. Lista corta a proposito; lo que no reconoce se marca generico.
fn vendor_for_mac(mac: &str) -> String {
    let clean: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() < 6 {
        return "Desconocido".to_string();
    }
    let prefix = clean[0..6].to_uppercase();
    let table: &[(&str, &str)] = &[
        ("F4F5D8", "Google"), ("3C5AB4", "Google"), ("A4772E", "Apple"),
        ("F0D1A9", "Apple"), ("AC87A3", "Apple"), ("00179A", "Apple"),
        ("341298", "Samsung"), ("8C7967", "Samsung"), ("A0C589", "Samsung"),
        ("B827EB", "Raspberry Pi"), ("DCA632", "Raspberry Pi"),
        ("00E04C", "Realtek"), ("001A11", "Google"),
        ("F81A67", "TP-Link"), ("50C7BF", "TP-Link"), ("EC086B", "TP-Link"),
        ("1CFAAA", "Huawei"), ("00259E", "Huawei"),
        ("A85E45", "Amazon"), ("FCA183", "Amazon"),
        ("D8BB2C", "Microsoft"), ("00155D", "Microsoft"),
        ("3496D7", "ASUS"), ("2C56DC", "ASUS"),
        ("C88D83", "Xiaomi"), ("64CC2E", "Xiaomi"),
    ];
    for (p, v) in table {
        if prefix.starts_with(p) {
            return v.to_string();
        }
    }
    "Desconocido".to_string()
}

/// Intento best-effort de resolver un nombre de host para la IP (reverse DNS
/// o NetBIOS local). Si falla, devuelve cadena vacia sin bloquear el escaneo.
fn resolve_hostname(ip: &str) -> String {
    if let Ok(addr) = ip.parse::<IpAddr>() {
        let sock = (addr, 0);
        if let Ok(mut iter) = sock.to_socket_addrs() {
            let _ = iter.next();
        }
    }
    // std no expone reverse-DNS directo y portable; en Windows nbtstat da el
    // nombre NetBIOS cuando el dispositivo lo anuncia (PCs, algunas smart TV).
    #[cfg(target_os = "windows")]
    {
        if let Ok(out) = Command::new("nbtstat").args(["-A", ip]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let t = line.trim();
                if t.contains("<00>") && t.contains("UNIQUE") {
                    if let Some(name) = t.split_whitespace().next() {
                        return name.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// Escanea la tabla ARP del sistema (poblada por el trafico normal de la red;
/// para refrescarla forzamos un ping broadcast previo no es necesario, ya que
/// arp -a basta para lo que ya "vio" el equipo, que en LAN domestica cubre
/// prácticamente todos los dispositivos activos).
#[cfg(target_os = "windows")]
fn read_arp_table() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(res) = Command::new("arp").arg("-a").output() else {
        return out;
    };
    let text = String::from_utf8_lossy(&res.stdout);
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Formato tipico: "  192.168.1.10          aa-bb-cc-dd-ee-ff     dynamic"
        if parts.len() >= 2 && parts[0].parse::<IpAddr>().is_ok() && parts[1].contains('-') {
            out.push((parts[0].to_string(), parts[1].to_string()));
        }
    }
    out
}

#[cfg(not(target_os = "windows"))]
fn read_arp_table() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(res) = Command::new("arp").arg("-a").output() else {
        return out;
    };
    let text = String::from_utf8_lossy(&res.stdout);
    for line in text.lines() {
        // Formato tipico macOS/Linux: "host (192.168.1.10) at aa:bb:cc:dd:ee:ff on en0 ..."
        if let (Some(ip_start), Some(ip_end)) = (line.find('('), line.find(')')) {
            let ip = &line[ip_start + 1..ip_end];
            if ip.parse::<IpAddr>().is_ok() {
                if let Some(at_pos) = line.find(" at ") {
                    let rest = &line[at_pos + 4..];
                    if let Some(mac) = rest.split_whitespace().next() {
                        out.push((ip.to_string(), mac.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Punto de entrada usado por el comando de Tauri. Deduplica por IP.
pub fn scan() -> Vec<NetDevice> {
    let mut seen: HashMap<String, NetDevice> = HashMap::new();
    for (ip, mac) in read_arp_table() {
        // Filtra entradas de broadcast/multicast que no son equipos reales.
        if ip.ends_with(".255") || mac.starts_with("01-00-5e") || mac == "ff-ff-ff-ff-ff-ff" {
            continue;
        }
        let vendor = vendor_for_mac(&mac);
        let hostname = resolve_hostname(&ip);
        seen.insert(
            ip.clone(),
            NetDevice { ip, mac, vendor, hostname },
        );
    }
    let mut list: Vec<NetDevice> = seen.into_values().collect();
    list.sort_by(|a, b| {
        let pa: Vec<u32> = a.ip.split('.').filter_map(|x| x.parse().ok()).collect();
        let pb: Vec<u32> = b.ip.split('.').filter_map(|x| x.parse().ok()).collect();
        pa.cmp(&pb)
    });
    list
}
