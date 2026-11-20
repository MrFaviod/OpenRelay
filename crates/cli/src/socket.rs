//! Unix control socket: server (one thread, one JSON line per request) and a
//! minimal client for the `openrelay` commands.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use openrelay_proto::control::ErrorCode;
use serde_json::{json, Value};

/// Maximum size of a request line.
const MAX_LINE: usize = 64 * 1024;
/// Maximum I/O wait per connection.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a read on the Unix socket may block before returning the turn.
/// The interleaved loop pumps QUIC on `Idle`; a full 5 s `read_line` would
/// leave the UDP endpoint unattended all that time.
const READ_SLICE: Duration = Duration::from_millis(20);

/// Result of an operation: `Ok(result)` or `Err(error)`. For control ops,
/// both are the complete v1 message (`ack`, `capabilities`, `error`).
pub type OpResult = Result<Value, Value>;

/// What the interleaved loop asks the process that owns the state for.
pub enum Interleave<'a> {
    /// No local client: pump the other transport.
    Idle,
    /// A line already authenticated by UID.
    Request {
        /// Operation.
        op: &'a str,
        /// Client `request_id`.
        request_id: &'a str,
        /// Operation arguments.
        args: &'a Value,
    },
}

/// Creates the parent directory 0700 and the socket 0600. If a live daemon is
/// already on `path`, returns an error; an orphan socket is removed.
///
/// `handler` receives [`Interleave::Idle`] on the same thread when there is no
/// local client. It serves to pump the process's QUIC without opening another
/// socket or another protocol. The listener is non-blocking.
pub fn serve_interleaved<H>(path: &Path, mut handler: H) -> io::Result<()>
where
    H: FnMut(Interleave) -> OpResult,
{
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path without a parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "a daemon is already listening on that socket",
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
                    eprintln!("openrelay: connection: {error}");
                }
            }
            Err(error) if is_wait(&error) => {
                let _ = handler(Interleave::Idle);
            }
            Err(error) => eprintln!("openrelay: accept: {error}"),
        }
    }
}

fn is_wait(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

/// Serves one connection: checks the peer UID and processes lines until EOF.
fn handle_conn<H>(stream: UnixStream, handler: &mut H) -> io::Result<()>
where
    H: FnMut(Interleave) -> OpResult,
{
    stream.set_read_timeout(Some(READ_SLICE))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    if !peer_is_self(&stream)? {
        // Immediate close: the peer is not our UID.
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
        // The cap is measured on the accumulated line, not on each read:
        // a timeout returns `Err` with the prefix already in `line`, so a
        // slow client could accumulate without limit if only `n` were checked.
        if line.trim_end_matches('\n').len() > MAX_LINE {
            let reply = error_reply(Value::Null, ErrorCode::TooLarge, "request too large");
            write_reply(reader.get_mut(), &reply)?;
            return Ok(());
        }
        match outcome {
            Ok(0) => return Ok(()),
            Ok(_) => {
                waited = Duration::ZERO;
                if !line.ends_with('\n') {
                    // The read cap cut mid-line: keep accumulating, without
                    // treating the prefix as a request.
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
                // Nobody completed the line: the other transport gets the turn.
                let _ = handler(Interleave::Idle);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Processes one line and returns the already formed reply.
fn process_line<H>(line: &str, handler: &mut H) -> Value
where
    H: FnMut(Interleave) -> OpResult,
{
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return error_reply(Value::Null, ErrorCode::Invalid, "invalid JSON"),
    };
    let id = match req.get("request_id") {
        Some(v) if v.is_string() => v.clone(),
        _ => return error_reply(Value::Null, ErrorCode::Invalid, "missing request_id"),
    };
    let op = match req.get("op").and_then(Value::as_str) {
        Some(op) => op,
        None => return error_reply(id, ErrorCode::Invalid, "missing op"),
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

/// `SO_PEERCRED` via rustix (no `unsafe`): is the peer UID ours?
fn peer_is_self(stream: &UnixStream) -> io::Result<bool> {
    let cred = rustix::net::sockopt::socket_peercred(stream)?;
    Ok(cred.uid.as_raw() == rustix::process::getuid().as_raw())
}

/// Sends a request `op` with `args` to the daemon at `path` and returns the
/// complete response.
pub fn request(path: &Path, op: &str, args: &Value) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => format!(
            "the daemon is not running (no socket at {}). Start it with: \
             systemctl --user enable --now openrelay.service   (or: openrelay daemon)",
            path.display()
        ),
        _ => format!("no daemon at {}: {e}", path.display()),
    })?;
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
        return Err("the daemon closed the connection without replying".to_owned());
    }
    let resp: Value =
        serde_json::from_str(line.trim_end()).map_err(|e| format!("invalid response: {e}"))?;
    if resp.get("request_id") != Some(&Value::String(id)) {
        return Err("response request_id does not match".to_owned());
    }
    Ok(resp)
}
