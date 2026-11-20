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
                "identity is not pending",
            ));
        }
        let mut doc = read_doc(path)?;
        let entries = doc["identities"]
            .as_array_mut()
            .ok_or_else(|| invalid("identities is not an array"))?;
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

/// Borra la identidad `pin` de `identities.json`, conservando el resto.
///
/// Un pin ausente no es error: el resultado es el mismo (sin entrada). La ACL
/// viva del servidor de sesión no se toca aquí: el llamador decide cuándo
/// revocar (`SessionServer::revoke`), porque el archivo y la memoria pueden
/// fallar por separado.
pub fn remove_identity(path: &Path, pin: &SpkiPin) -> io::Result<()> {
    let mut doc = read_doc(path)?;
    let entries = doc["identities"]
        .as_array_mut()
        .ok_or_else(|| invalid("identities is not an array"))?;
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
            .ok_or_else(|| invalid("invalid spki"))?;
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
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|_| invalid("invalid JSON"))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            json!({"version": IDENTITIES_VERSION, "identities": []})
        }
        Err(error) => return Err(error),
    };
    if doc["version"].as_u64() != Some(IDENTITIES_VERSION) || !doc["identities"].is_array() {
        return Err(invalid("unknown identities version or shape"));
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
    let bytes = serde_json::to_vec_pretty(doc).map_err(|_| invalid("serialization"))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("openrelay-pairing-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("directorio de prueba");
        dir.join("identities.json")
    }

    fn pin(byte: u8) -> SpkiPin {
        SpkiPin::from_spki_der(&[byte; 40])
    }

    #[test]
    fn remove_identity_borra_solo_el_pin_pedido_y_es_idempotente() {
        let path = scratch("remove");
        let (removed, kept) = (pin(1), pin(2));
        let doc = json!({
            "version": IDENTITIES_VERSION,
            "identities": [
                {"spki": removed.to_hex(), "mic": true, "control_read": false,
                 "control_write": false, "return_audio": false},
                {"spki": kept.to_hex(), "mic": true, "control_read": true,
                 "control_write": false, "return_audio": false},
            ],
        });
        fs::write(&path, serde_json::to_vec(&doc).expect("json")).expect("identidades");

        remove_identity(&path, &removed).expect("borrar");
        let acl = load(&path).expect("cargar");
        assert!(
            acl.capabilities(&removed).is_none(),
            "el pin borrado no vuelve"
        );
        assert!(acl.capabilities(&kept).is_some(), "el resto se conserva");

        // Un segundo remove del mismo pin no es error.
        remove_identity(&path, &removed).expect("borrar dos veces");
        let doc: Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("leer")).expect("json");
        assert_eq!(doc["identities"].as_array().map(Vec::len), Some(1));
    }
}
