//! Identidad persistente del equipo.
//!
//! No hay codigo de activacion: al primer arranque se genera automaticamente
//! un ID propio de 9 digitos y se guarda en el directorio de datos de la app.
//! El acceso desatendido queda activo por defecto. La contrasena de sesion es
//! efimera (4 digitos) y se regenera cuando el usuario lo pide.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Estado persistido en disco (identity.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// ID unico del equipo, 9 digitos, estable entre arranques.
    pub id: String,
    /// Nombre visible del puesto (por defecto el hostname).
    pub device_name: String,
    /// Acceso desatendido activo (permite reconexion sin aprobacion manual).
    pub unattended: bool,
    /// Marca de tiempo (epoch, s) de creacion.
    pub created_at: u64,
}

impl Identity {
    fn generate(device_name: String) -> Self {
        Identity {
            id: generate_numeric_id(9),
            device_name,
            unattended: true,
            created_at: now_secs(),
        }
    }
}

/// Ruta del fichero de identidad dentro del dir de datos de la app.
fn identity_path(base: &PathBuf) -> PathBuf {
    base.join("identity.json")
}

/// Carga la identidad existente o crea una nueva y la persiste.
pub fn load_or_create(app_data_dir: &PathBuf, default_device_name: String) -> Identity {
    let path = identity_path(app_data_dir);
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(id) = serde_json::from_slice::<Identity>(&bytes) {
            return id;
        }
    }
    let identity = Identity::generate(default_device_name);
    let _ = std::fs::create_dir_all(app_data_dir);
    if let Ok(json) = serde_json::to_vec_pretty(&identity) {
        let _ = std::fs::write(&path, json);
    }
    identity
}

/// Persiste cambios en la identidad (p.ej. renombrar el puesto).
pub fn save(app_data_dir: &PathBuf, identity: &Identity) {
    let _ = std::fs::create_dir_all(app_data_dir);
    if let Ok(json) = serde_json::to_vec_pretty(identity) {
        let _ = std::fs::write(identity_path(app_data_dir), json);
    }
}

/// Genera una contrasena de sesion de 4 digitos.
pub fn generate_session_password() -> String {
    generate_numeric_id(4)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// PRNG ligero sin dependencias externas: mezcla el reloj de alta resolucion
/// con un contador para producir digitos. Suficiente para IDs de sesion.
fn generate_numeric_id(digits: usize) -> String {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15)
        ^ (std::process::id() as u64).rotate_left(17);

    let mut out = String::with_capacity(digits);
    for i in 0..digits {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let mut d = (state % 10) as u8;
        // El primer digito nunca es 0, para mantener 9 cifras reales.
        if i == 0 && d == 0 {
            d = 1;
        }
        out.push((b'0' + d) as char);
    }
    out
}
