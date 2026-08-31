//! Perfilado real del equipo (no datos de ejemplo).
//!
//! - RAM total, marca de CPU y nucleos via `sysinfo`.
//! - Version de Windows via registro (windows-rs family: `windows-registry`).
//! - Metricas en vivo del propio proceso cliente (RAM MB, CPU %).

use serde::Serialize;
use std::sync::Mutex;
use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

/// Umbral de "gama baja": <= 4.5 GiB de RAM o CPU de bajo consumo conocida.
const LOW_END_RAM_BYTES: u64 = 4_800_000_000;

/// Perfil de calidad por defecto que se aplica segun el hardware.
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityProfile {
    Ultralight,
    Balanced,
}

/// Instantanea estatica del equipo, calculada una vez al arrancar.
#[derive(Serialize)]
pub struct SystemProfile {
    pub hostname: String,
    pub os_name: String,
    pub cpu_brand: String,
    pub cpu_cores: usize,
    pub total_ram_mb: u64,
    pub is_low_end: bool,
    pub default_profile: QualityProfile,
}

/// Metricas en vivo del proceso cliente (se refrescan por polling desde la UI).
#[derive(Serialize)]
pub struct ClientMetrics {
    pub client_ram_mb: u64,
    pub client_cpu_pct: f32,
}

/// Estado compartido: mantiene un `System` reutilizable para no re-escanear
/// todo el sistema en cada tick (mas barato en CPU en equipos lentos).
pub struct Monitor {
    sys: Mutex<System>,
    pid: Pid,
}

impl Monitor {
    pub fn new() -> Self {
        let pid = Pid::from_u32(std::process::id());
        let sys = System::new_with_specifics(
            RefreshKind::new()
                .with_processes(ProcessRefreshKind::new().with_cpu().with_memory()),
        );
        Monitor {
            sys: Mutex::new(sys),
            pid,
        }
    }

    /// Instantanea estatica del hardware. Se llama una vez.
    pub fn profile(&self) -> SystemProfile {
        let mut sys = System::new_with_specifics(
            RefreshKind::new().with_cpu(sysinfo::CpuRefreshKind::everything()),
        );
        sys.refresh_memory();
        sys.refresh_cpu_all();

        let total_ram_mb = sys.total_memory() / (1024 * 1024);
        let cpu_brand = sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "CPU desconocida".to_string());
        let cpu_cores = sys.cpus().len();

        let low_by_ram = sys.total_memory() <= LOW_END_RAM_BYTES;
        let low_by_cpu = {
            let b = cpu_brand.to_lowercase();
            b.contains("celeron") || b.contains("atom") || b.contains("pentium")
        };
        let is_low_end = low_by_ram || low_by_cpu;

        SystemProfile {
            hostname: System::host_name().unwrap_or_else(|| "Mi PC".to_string()),
            os_name: windows_os_name(),
            cpu_brand,
            cpu_cores,
            total_ram_mb,
            is_low_end,
            default_profile: if is_low_end {
                QualityProfile::Ultralight
            } else {
                QualityProfile::Balanced
            },
        }
    }

    /// Metricas en vivo del propio proceso cliente.
    pub fn client_metrics(&self) -> ClientMetrics {
        let mut sys = self.sys.lock().unwrap();
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::new().with_cpu().with_memory(),
        );

        if let Some(proc_) = sys.process(self.pid) {
            let ram_mb = proc_.memory() / (1024 * 1024);
            // CPU % del proceso normalizado al total de nucleos (0-100 global).
            let cores = sys.cpus().len().max(1) as f32;
            let cpu_pct = (proc_.cpu_usage() / cores).clamp(0.0, 100.0);
            ClientMetrics {
                client_ram_mb: ram_mb,
                client_cpu_pct: (cpu_pct * 10.0).round() / 10.0,
            }
        } else {
            ClientMetrics {
                client_ram_mb: 0,
                client_cpu_pct: 0.0,
            }
        }
    }
}

/// Lee el nombre de producto real de Windows del registro.
#[cfg(windows)]
fn windows_os_name() -> String {
    use windows_registry::LOCAL_MACHINE;

    let key = match LOCAL_MACHINE.open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion") {
        Ok(k) => k,
        Err(_) => return "Windows".to_string(),
    };

    let product = key.get_string("ProductName").unwrap_or_default();
    let display = key.get_string("DisplayVersion").unwrap_or_default();

    // Windows 11 se reporta como "Windows 10 ..." en ProductName; corregir por build.
    let build: u32 = key
        .get_string("CurrentBuildNumber")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut name = if build >= 22000 && product.contains("Windows 10") {
        product.replace("Windows 10", "Windows 11")
    } else if product.is_empty() {
        "Windows".to_string()
    } else {
        product
    };

    if !display.is_empty() {
        name.push(' ');
        name.push_str(&display);
    }
    name
}

#[cfg(not(windows))]
fn windows_os_name() -> String {
    System::long_os_version().unwrap_or_else(|| "Sistema".to_string())
}
