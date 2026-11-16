//! `arl` — CLI local de administración del daemon de arl.
//!
//! Uso:
//!   arl daemon [--socket PATH]   arranca el servidor de control en primer plano
//!   arl status [--socket PATH]   pide el estado al daemon y lo imprime (JSON)
//!
//! Códigos de salida: 0 correcto; 1 el daemon no responde o devuelve error;
//! 2 uso incorrecto.

mod socket;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use arl_proto::control::ErrorCode;
use serde_json::{json, Value};

use socket::OpResult;

const USAGE: &str = "\
arl — CLI de administración del daemon de arl

Uso:
  arl daemon [--socket PATH]   arranca el servidor de control (primer plano)
  arl status [--socket PATH]   muestra el estado del daemon (JSON)
  arl --help | arl <subcomando> --help

Socket por defecto: $XDG_RUNTIME_DIR/arl/control.sock (0700 dir, 0600 socket).
";

const DAEMON_USAGE: &str = "\
Uso: arl daemon [--socket PATH]

Arranca el servidor de control en primer plano. Sólo acepta conexiones del
mismo UID. Ctrl-C para detenerlo.
";

const STATUS_USAGE: &str = "\
Uso: arl status [--socket PATH]

Imprime la respuesta JSON completa del daemon. Sale con 1 si no responde.
";

/// Estado del daemon. Punto de extensión: las ops nuevas leen/escriben aquí.
pub struct State {
    started: Instant,
    sessions: u64,
}

/// Hook de operaciones: despacha `op` con sus `args` sobre el `State`.
pub fn handle(op: &str, _args: &Value, state: &mut State) -> OpResult {
    match op {
        "status" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_s": state.started.elapsed().as_secs(),
            "sessions": state.sessions,
        })),
        other => Err((ErrorCode::Unsupported, format!("op desconocida: {other}"))),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let sub = match args.first().map(String::as_str) {
        None | Some("--help") | Some("-h") => {
            print!("{USAGE}");
            return ExitCode::from(if args.is_empty() { 2 } else { 0 });
        }
        Some(s) => s,
    };
    let rest = &args[1..];

    match sub {
        "daemon" => cmd_daemon(rest),
        "status" => cmd_status(rest),
        other => {
            eprintln!("arl: subcomando desconocido: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Parsea `[--socket PATH]` y `--help`. `Err(code)` si ya se debe salir.
fn parse_socket_opts(rest: &[String], usage: &str) -> Result<PathBuf, ExitCode> {
    let mut socket: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{usage}");
                return Err(ExitCode::SUCCESS);
            }
            "--socket" => match it.next() {
                Some(p) => socket = Some(PathBuf::from(p)),
                None => {
                    eprintln!("arl: --socket requiere una ruta");
                    return Err(ExitCode::from(2));
                }
            },
            other => {
                eprintln!("arl: argumento desconocido: {other}\n\n{usage}");
                return Err(ExitCode::from(2));
            }
        }
    }
    match socket {
        Some(p) => Ok(p),
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir).join("arl").join("control.sock")),
            _ => {
                eprintln!("arl: define --socket o XDG_RUNTIME_DIR");
                Err(ExitCode::from(2))
            }
        },
    }
}

fn cmd_daemon(rest: &[String]) -> ExitCode {
    let path = match parse_socket_opts(rest, DAEMON_USAGE) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let mut state = State {
        started: Instant::now(),
        sessions: 0,
    };
    eprintln!("arl: daemon escuchando en {}", path.display());
    match socket::serve(&path, |op, args| handle(op, args, &mut state)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("arl: daemon: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_status(rest: &[String]) -> ExitCode {
    let path = match parse_socket_opts(rest, STATUS_USAGE) {
        Ok(p) => p,
        Err(code) => return code,
    };
    match socket::request(&path, "status") {
        Ok(resp) => {
            println!("{resp}");
            if resp.get("ok") == Some(&Value::Bool(true)) {
                ExitCode::SUCCESS
            } else {
                eprintln!("arl: el daemon devolvió error");
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("arl: {e}");
            ExitCode::from(1)
        }
    }
}
