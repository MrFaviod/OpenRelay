//! Socket Unix de control: servidor (un hilo, una línea JSON por petición) y
//! cliente mínimo para `arl status`.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use arl_proto::control::ErrorCode;
use serde_json::{json, Value};

/// Tamaño máximo de una línea de petición.
const MAX_LINE: usize = 64 * 1024;
/// Tiempo máximo de espera de E/S por conexión.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Resultado de una operación: `Ok(result)` o `(código, mensaje)`.
pub type OpResult = Result<Value, (ErrorCode, String)>;

/// Sirve peticiones en `path` hasta que el proceso termina.
///
/// Crea el directorio padre 0700 y el socket 0600. Si ya hay un daemon vivo
/// en `path`, devuelve error; un socket huérfano se elimina.
pub fn serve<H>(path: &Path, mut handler: H) -> io::Result<()>
where
    H: FnMut(&str, &Value) -> OpResult,
{
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "ruta de socket sin directorio")
    })?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "ya hay un daemon escuchando en ese socket",
            ));
        }
        fs::remove_file(path)?;
    }

    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                if let Err(e) = handle_conn(stream, &mut handler) {
                    eprintln!("arl: conexión: {e}");
                }
            }
            Err(e) => eprintln!("arl: accept: {e}"),
        }
    }
    Ok(())
}

/// Atiende una conexión: comprueba el UID del par y procesa líneas hasta EOF.
fn handle_conn<H>(stream: UnixStream, handler: &mut H) -> io::Result<()>
where
    H: FnMut(&str, &Value) -> OpResult,
{
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    if !peer_is_self(&stream)? {
        // Cierre inmediato: el par no es nuestro UID.
        return Ok(());
    }

    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if n > MAX_LINE && !line.ends_with('\n') {
            let reply = error_reply(
                Value::Null,
                ErrorCode::TooLarge,
                "petición demasiado grande",
            );
            write_reply(reader.get_mut(), &reply)?;
            return Ok(());
        }
        let reply = process_line(line.trim_end(), handler);
        write_reply(reader.get_mut(), &reply)?;
    }
}

/// Procesa una línea y devuelve la respuesta ya formada.
fn process_line<H>(line: &str, handler: &mut H) -> Value
where
    H: FnMut(&str, &Value) -> OpResult,
{
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return error_reply(Value::Null, ErrorCode::Invalid, "JSON inválido"),
    };
    let id = match req.get("request_id") {
        Some(v) if v.is_string() => v.clone(),
        _ => return error_reply(Value::Null, ErrorCode::Invalid, "falta request_id"),
    };
    let op = match req.get("op").and_then(Value::as_str) {
        Some(op) => op,
        None => return error_reply(id, ErrorCode::Invalid, "falta op"),
    };
    let args = req.get("args").cloned().unwrap_or(Value::Null);
    match handler(op, &args) {
        Ok(result) => json!({"request_id": id, "ok": true, "result": result}),
        Err((code, message)) => error_reply(id, code, &message),
    }
}

fn error_reply(id: Value, code: ErrorCode, message: &str) -> Value {
    json!({
        "request_id": id,
        "ok": false,
        "error": {"code": code.as_str(), "message": message},
    })
}

fn write_reply(stream: &mut UnixStream, reply: &Value) -> io::Result<()> {
    let mut out = reply.to_string();
    out.push('\n');
    stream.write_all(out.as_bytes())
}

/// `SO_PEERCRED` vía rustix (sin `unsafe`): ¿el UID del par es el nuestro?
fn peer_is_self(stream: &UnixStream) -> io::Result<bool> {
    let cred = rustix::net::sockopt::socket_peercred(stream)?;
    Ok(cred.uid.as_raw() == rustix::process::getuid().as_raw())
}

/// Envía una petición `op` al daemon en `path` y devuelve la respuesta completa.
pub fn request(path: &Path, op: &str) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path)
        .map_err(|e| format!("no hay daemon en {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| e.to_string())?;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let id = format!("cli-{}-{}", std::process::id(), nanos);
    let mut req = json!({"request_id": id, "op": op}).to_string();
    req.push('\n');
    stream
        .write_all(req.as_bytes())
        .map_err(|e| e.to_string())?;

    let mut line = String::new();
    let n = BufReader::new(Read::take(stream, MAX_LINE as u64))
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Err("el daemon cerró la conexión sin responder".to_owned());
    }
    let resp: Value =
        serde_json::from_str(line.trim_end()).map_err(|e| format!("respuesta inválida: {e}"))?;
    if resp.get("request_id") != Some(&Value::String(id)) {
        return Err("request_id de la respuesta no coincide".to_owned());
    }
    Ok(resp)
}
