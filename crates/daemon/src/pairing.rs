//! Emparejamiento local: QR con pin y token -> enrolamiento -> pendiente ->
//! aprobación explícita -> identidad persistente en la ACL.
//!
//! Nada se escribe en disco antes de [`Pairing::approve`]. Los pendientes viven
//! sólo en memoria: si el daemon se reinicia, el cliente vuelve a enrolar.

use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use openrelay_transport::enrollment::{EnrollmentEvent, EnrollmentToken};
use openrelay_transport::identity::SpkiPin;
use openrelay_transport::quinn::{Acl, Capabilities};
use serde_json::{json, Value};

const IDENTITIES_VERSION: u64 = 1;

/// Estado de emparejamiento del daemon.
#[derive(Debug, Default)]
pub struct Pairing {
    pending: Vec<SpkiPin>,
}

impl Pairing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Contenido del QR: `openrelay://pair?v=1&h=<host:port>&pin=<hex>&t=<hex>`.
    pub fn qr_payload(server_pin: SpkiPin, addr: SocketAddr, token: &EnrollmentToken) -> String {
        format!(
            "openrelay://pair?v=1&h={addr}&pin={}&t={}",
            server_pin.to_hex(),
            token.to_hex()
        )
    }

    /// Alimenta los pendientes con los eventos del enrolamiento.
    pub fn observe(&mut self, event: &EnrollmentEvent) {
        if let EnrollmentEvent::Accepted(pin) = event {
            if !self.pending.contains(pin) {
                self.pending.push(*pin);
            }
        }
    }

    /// Identidades enroladas que esperan aprobación local.
    pub fn pending(&self) -> &[SpkiPin] {
        &self.pending
    }

    /// Aprueba una identidad pendiente y la escribe en `path`.
    pub fn approve(&mut self, pin: SpkiPin, caps: Capabilities, path: &Path) -> io::Result<()> {
        if !self.pending.contains(&pin) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "identidad no pendiente",
            ));
        }
        let mut doc = read_doc(path)?;
        let entries = doc["identities"]
            .as_array_mut()
            .ok_or_else(|| invalid("identities no es array"))?;
        entries.retain(|entry| entry["spki"].as_str() != Some(&pin.to_hex()));
        entries.push(json!({
            "spki": pin.to_hex(),
            "mic": caps.mic(),
            "control_read": caps.control_read(),
            "control_write": caps.control_write(),
            "return_audio": caps.return_audio(),
        }));
        write_atomic(path, &doc)?;
        self.pending.retain(|item| item != &pin);
        Ok(())
    }

    /// Descarta una identidad pendiente sin escribir nada.
    pub fn reject(&mut self, pin: SpkiPin) {
        self.pending.retain(|item| item != &pin);
    }

    /// Vacía los pendientes: la ventana se cerró y ninguno sigue en juego.
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }
}

/// Borra una identidad del archivo de identidades (escritura atómica).
/// Un pin ausente no es error: el resultado es el mismo (sin entrada).
pub(crate) fn remove_identity(path: &Path, pin: &SpkiPin) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let entries = doc["identities"]
        .as_array_mut()
        .ok_or_else(|| invalid("identities no es array"))?;
    entries.retain(|entry| entry["spki"].as_str() != Some(&pin.to_hex()));
    write_atomic(path, &doc)
}

/// Carga la ACL persistida. Un archivo inexistente es ACL vacía (default deny).
pub fn load(path: &Path) -> io::Result<Acl> {
    let doc = read_doc(path)?;
    let mut acl = Acl::new();
    for entry in doc["identities"].as_array().into_iter().flatten() {
        let pin = entry["spki"]
            .as_str()
            .and_then(SpkiPin::from_hex)
            .ok_or_else(|| invalid("spki inválido"))?;
        let flag = |name: &str| entry[name].as_bool().unwrap_or(false);
        let mut caps = Capabilities::none();
        if flag("mic") {
            caps = caps.with_mic();
        }
        if flag("control_read") {
            caps = caps.with_control_read();
        }
        if flag("control_write") {
            caps = caps.with_control_write();
        }
        if flag("return_audio") {
            caps = caps.with_return_audio();
        }
        acl.allow(pin, caps);
    }
    Ok(acl)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_doc(path: &Path) -> io::Result<Value> {
    let doc = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|_| invalid("JSON inválido"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            json!({"version": IDENTITIES_VERSION, "identities": []})
        }
        Err(error) => return Err(error),
    };
    if doc["version"].as_u64() != Some(IDENTITIES_VERSION) || !doc["identities"].is_array() {
        return Err(invalid("versión o forma de identidades desconocida"));
    }
    Ok(doc)
}

/// Escribe `doc` en `path` vía tmp (0600, sin sobrescribir) + rename.
fn write_atomic(path: &Path, doc: &Value) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    let _ = fs::remove_file(tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    let bytes = serde_json::to_vec_pretty(doc).map_err(|_| invalid("serialización"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)
}
