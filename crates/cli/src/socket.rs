//! Socket Unix de control: servidor (un hilo, una línea JSON por petición) y
//! cliente mínimo para las órdenes de `arl`.

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
/// Cuánto puede bloquear una lectura del socket Unix antes de devolver el
/// turno. El bucle intercalado bombea QUIC en `Idle`; un `read_line` de los
/// 5 s completos dejaría el UDP sin atender durante todo ese tiempo.
const READ_SLICE: Duration = Duration::from_millis(20);

/// Resultado de una operación: `Ok(result)` o `Err(error)`. Para las ops de
/// control, ambos son el mensaje v1 completo (`ack`, `capabilities`, `error`).
pub type OpResult = Result<Value, Value>;

/// Qué pide el bucle intercalado al proceso dueño del estado.
pub enum Interleave<'a> {
    /// No hay cliente local: bombear el otro transporte.
    Idle,
    /// Una línea ya autenticada por UID.
    Request {
        /// Operación.
        op: &'a str,
        /// `request_id` del cliente.
        request_id: &'a str,
        /// Argumentos de la operación.
        args: &'a Value,
    },
}

/// Crea el directorio padre 0700 y el socket 0600. Si ya hay un daemon vivo
/// en `path`, devuelve error; un socket huérfano se elimina.
///
/// `handler` recibe [`Interleave::Idle`] en el mismo hilo cuando no hay un
/// cliente local. Sirve para bombear el QUIC del proceso sin abrir otro
/// socket ni otro protocolo. El listener no es bloqueante.
pub fn serve_interleaved<H>(path: &Path, mut handler: H) -> io::Result<()>
where
    H: FnMut(Interleave) -> OpResult,
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
    listener.set_nonblocking(true)?;
    serve_listener(&listener, &mut handler)
}

fn serve_listener<H>(listener: &UnixListener, handler: &mut H) -> io::Result<()>
where
    H: FnMut(Interleave) -> OpResult,
{
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(error) = handle_conn(stream, handler) {
                    eprintln!("arl: conexión: {error}");
                }
            }
            Err(error) if is_wait(&error) => {
                let _ = handler(Interleave::Idle);
            }
            Err(error) => eprintln!("arl: accept: {error}"),
        }
    }
}

fn is_wait(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Atiende una conexión: comprueba el UID del par y procesa líneas hasta EOF.
fn handle_conn<H>(stream: UnixStream, handler: &mut H) -> io::Result<()>
where
    H: FnMut(Interleave) -> OpResult,
{
    stream.set_read_timeout(Some(READ_SLICE))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    if !peer_is_self(&stream)? {
        // Cierre inmediato: el par no es nuestro UID.
        return Ok(());
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut waited = Duration::ZERO;
    loop {
        let outcome = reader
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut line);
        // El tope se mide sobre la línea acumulada, no sobre cada lectura:
        // un timeout devuelve `Err` con el prefijo ya en `line`, así que un
        // cliente lento podría acumular sin límite si sólo se mirara `n`.
        if line.trim_end_matches('\n').len() > MAX_LINE {
            let reply = error_reply(
                Value::Null,
                ErrorCode::TooLarge,
                "petición demasiado grande",
            );
            write_reply(reader.get_mut(), &reply)?;
            return Ok(());
        }
        match outcome {
            Ok(0) => return Ok(()),
            Ok(_) => {
                waited = Duration::ZERO;
                if !line.ends_with('\n') {
                    // El tope de lectura cortó a mitad de línea: se sigue
                    // acumulando, sin tratar el prefijo como una petición.
                    continue;
                }
                let reply = process_line(line.trim_end(), handler);
                line.clear();
                write_reply(reader.get_mut(), &reply)?;
            }
            Err(error) if is_wait(&error) => {
                waited = waited.saturating_add(READ_SLICE);
                if waited >= IO_TIMEOUT {
                    return Err(error);
                }
                // Nadie completó la línea: el otro transporte recupera el turno.
                let _ = handler(Interleave::Idle);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Procesa una línea y devuelve la respuesta ya formada.
fn process_line<H>(line: &str, handler: &mut H) -> Value
where
    H: FnMut(Interleave) -> OpResult,
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
    let id_text = id.as_str().unwrap_or_default();
    match handler(Interleave::Request {
        op,
        request_id: id_text,
        args: &args,
    }) {
        Ok(result) => json!({"request_id": id, "ok": true, "result": result}),
        Err(error) => json!({"request_id": id, "ok": false, "error": error}),
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

/// Envía una petición `op` con `args` al daemon en `path` y devuelve la
/// respuesta completa.
pub fn request(path: &Path, op: &str, args: &Value) -> Result<Value, String> {
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
    let mut req = json!({"request_id": id, "op": op, "args": args}).to_string();
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
