//! `arl` — CLI local de administración del daemon de arl.
//!
//! Uso:
//!
//! ```text
//! arl daemon [--socket PATH]            arranca el servidor de control en primer plano
//! arl status [--socket PATH]            pide el estado al daemon y lo imprime (JSON)
//! arl config get [KEY] [--socket PATH]  lee una clave o el snapshot completo
//! arl config set KEY VALUE --rev N      escribe una clave con revisión esperada
//! arl capabilities [--socket PATH]      pide las capacidades y la revisión actual
//! ```
//!
//! Códigos de salida: 0 correcto; 1 el daemon no responde o devuelve error;
//! 2 uso incorrecto.

mod socket;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arl_proto::control::{
    AuthorizedPeer, ConfigKey, ConfigState, ConfigValue, ControlSession, ErrorCode, IdentityPolicy,
    PeerRole, RemoteAction,
};
use arl_proto::wire::{
    decode_frame, encode_message, ConfigGetMsg, ConfigSetMsg, ControlMessage, HelloMsg,
    SCHEMA_VERSION,
};
use serde_json::{json, Value};

use socket::OpResult;

const USAGE: &str = "\
arl — CLI de administración del daemon de arl

Uso:
  arl daemon [--socket PATH]            arranca el servidor de control (primer plano)
  arl status [--socket PATH]            muestra el estado del daemon (JSON)
  arl config get [KEY] [--socket PATH]  lee una clave o todas (JSON)
  arl config set KEY VALUE --rev N      escribe una clave (JSON)
  arl capabilities [--socket PATH]      capacidades y revisión actual (JSON)
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

const CONFIG_USAGE: &str = "\
Uso:
  arl config get [KEY] [--socket PATH]
  arl config set KEY VALUE --rev N [--socket PATH]

get sin KEY devuelve el snapshot completo. set exige --rev con la revisión
que leíste antes; si no coincide, sale con 1 y devuelve error `conflict` con
`current_revision`. VALUE se interpreta como true/false, entero, decimal o
texto. Un mismo request_id repetido devuelve la misma respuesta sin aplicar
dos veces.
";

const CAPABILITIES_USAGE: &str = "\
Uso: arl capabilities [--socket PATH]

Imprime el mensaje `capabilities` del daemon (revisión, claves y límites).
";

/// Estado del daemon: sesión de control revisionada (fuente de verdad de la
/// configuración) más contadores propios.
pub struct State {
    started: Instant,
    sessions: u64,
    control: ControlSession,
}

impl State {
    fn new() -> Self {
        // El admin local tiene ambas acciones; el rol linux-cli no es remoto,
        // así que las claves locales también son escribibles desde aquí.
        let peer = AuthorizedPeer::new(
            "arl-cli-local",
            PeerRole::LinuxCli,
            vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
        );
        Self {
            started: Instant::now(),
            sessions: 0,
            control: ControlSession::new(peer, ConfigState::new(), Box::new(IdentityPolicy)),
        }
    }
}

/// Hook de operaciones: despacha `op` con sus `args` sobre el `State`.
///
/// `Ok` lleva el resultado; `Err` lleva el objeto `error` (`code`, `message`,
/// `retryable`, y `current_revision` en conflictos). Las ops de control
/// reutilizan `ControlSession`: revisión, conflicto e idempotencia viven allí.
pub fn handle(op: &str, request_id: &str, args: &Value, state: &mut State) -> OpResult {
    match op {
        "status" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_s": state.started.elapsed().as_secs(),
            "sessions": state.sessions,
        })),
        "config.get" => {
            let key = match args.get("key") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) => Some(parse_key(name)?),
                Some(_) => return Err(failure(ErrorCode::Invalid, "key debe ser texto")),
            };
            control(
                state,
                ControlMessage::ConfigGet(ConfigGetMsg {
                    request_id: request_id.to_owned(),
                    key,
                }),
            )
        }
        "config.set" => {
            let key = match args.get("key").and_then(Value::as_str) {
                Some(name) => parse_key(name)?,
                None => return Err(failure(ErrorCode::Invalid, "falta key")),
            };
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| failure(ErrorCode::Invalid, "falta expected_revision"))?;
            let value = match args.get("value") {
                Some(Value::Bool(b)) => ConfigValue::Bool(*b),
                Some(Value::String(s)) => ConfigValue::String(s.clone()),
                Some(Value::Number(n)) => match (n.as_i64(), n.as_f64()) {
                    (Some(i), _) => ConfigValue::Integer(i),
                    (None, Some(f)) => ConfigValue::Number(f),
                    _ => return Err(failure(ErrorCode::Invalid, "value no representable")),
                },
                _ => {
                    return Err(failure(
                        ErrorCode::Invalid,
                        "value debe ser bool, número o texto",
                    ))
                }
            };
            control(
                state,
                ControlMessage::ConfigSet(ConfigSetMsg {
                    request_id: request_id.to_owned(),
                    expected_revision,
                    key,
                    value,
                }),
            )
        }
        "capabilities" => control(
            state,
            ControlMessage::Hello(HelloMsg {
                request_id: request_id.to_owned(),
                role: PeerRole::LinuxCli,
                version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol_version: SCHEMA_VERSION.to_owned(),
            }),
        ),
        other => Err(failure(
            ErrorCode::Unsupported,
            &format!("op desconocida: {other}"),
        )),
    }
}

/// Pasa un mensaje por la sesión de control y devuelve su respuesta en JSON
/// (el mismo envelope que el wire de `arl.control/v1`).
fn control(state: &mut State, message: ControlMessage) -> OpResult {
    // La petición se valida con las reglas del wire antes de tocar el estado:
    // request_id, clave, tipo y rango. Así un error de forma nunca muta.
    if encode_message(&message).is_err() {
        return Err(failure(ErrorCode::Invalid, "petición de control inválida"));
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let outcome = state.control.handle(&message, now_ms);
    let Some(response) = outcome.response else {
        return Err(failure(ErrorCode::Internal, "sin respuesta de control"));
    };
    let envelope = to_json(&response).map_err(|m| failure(ErrorCode::Internal, &m))?;
    match response {
        ControlMessage::Error(_) => Err(envelope["payload"].clone()),
        _ => Ok(envelope),
    }
}

/// Serializa un mensaje de control a su envelope JSON (sin el prefijo de longitud).
fn to_json(message: &ControlMessage) -> Result<Value, String> {
    let frame =
        encode_message(message).map_err(|_| "mensaje de control no codificable".to_owned())?;
    let payload = decode_frame(&frame).map_err(|_| "frame de control inválido".to_owned())?;
    serde_json::from_slice(payload).map_err(|_| "JSON de control inválido".to_owned())
}

fn parse_key(name: &str) -> Result<ConfigKey, Value> {
    ConfigKey::new(name).map_err(|code| failure(code, "clave de configuración inválida"))
}

fn failure(code: ErrorCode, message: &str) -> Value {
    json!({"code": code.as_str(), "message": message, "retryable": code.retryable()})
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
        "config" => cmd_config(rest),
        "capabilities" => cmd_capabilities(rest),
        other => {
            eprintln!("arl: subcomando desconocido: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Opciones comunes ya separadas de los argumentos posicionales.
struct Opts {
    socket: PathBuf,
    rev: Option<u64>,
    positional: Vec<String>,
}

/// Parsea `[--socket PATH]`, `--rev N` (sólo si `rev_allowed`), posicionales y
/// `--help`. `Err(code)` si ya se debe salir.
fn parse_opts(rest: &[String], usage: &str, rev_allowed: bool) -> Result<Opts, ExitCode> {
    let mut socket: Option<PathBuf> = None;
    let mut rev: Option<u64> = None;
    let mut positional = Vec::new();
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
            "--rev" if rev_allowed => match it.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => rev = Some(n),
                None => {
                    eprintln!("arl: --rev requiere un entero no negativo");
                    return Err(ExitCode::from(2));
                }
            },
            other if other.starts_with('-') && other.len() > 1 && other.parse::<f64>().is_err() => {
                eprintln!("arl: argumento desconocido: {other}\n\n{usage}");
                return Err(ExitCode::from(2));
            }
            other => positional.push(other.to_owned()),
        }
    }
    let socket = match socket {
        Some(p) => p,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("arl").join("control.sock"),
            _ => {
                eprintln!("arl: define --socket o XDG_RUNTIME_DIR");
                return Err(ExitCode::from(2));
            }
        },
    };
    Ok(Opts {
        socket,
        rev,
        positional,
    })
}

/// Opciones sin posicionales ni `--rev` (daemon, status, capabilities).
fn parse_plain(rest: &[String], usage: &str) -> Result<PathBuf, ExitCode> {
    let opts = parse_opts(rest, usage, false)?;
    if !opts.positional.is_empty() {
        eprintln!(
            "arl: argumento desconocido: {}\n\n{usage}",
            opts.positional[0]
        );
        return Err(ExitCode::from(2));
    }
    Ok(opts.socket)
}

fn cmd_daemon(rest: &[String]) -> ExitCode {
    let path = match parse_plain(rest, DAEMON_USAGE) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let mut state = State::new();
    eprintln!("arl: daemon escuchando en {}", path.display());
    match socket::serve(&path, |op, id, args| handle(op, id, args, &mut state)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("arl: daemon: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_status(rest: &[String]) -> ExitCode {
    match parse_plain(rest, STATUS_USAGE) {
        Ok(path) => send(&path, "status", &Value::Null),
        Err(code) => code,
    }
}

fn cmd_capabilities(rest: &[String]) -> ExitCode {
    match parse_plain(rest, CAPABILITIES_USAGE) {
        Ok(path) => send(&path, "capabilities", &Value::Null),
        Err(code) => code,
    }
}

fn cmd_config(rest: &[String]) -> ExitCode {
    match rest.first().map(String::as_str) {
        Some("--help") | Some("-h") => {
            print!("{CONFIG_USAGE}");
            ExitCode::SUCCESS
        }
        Some("get") => cmd_config_get(&rest[1..]),
        Some("set") => cmd_config_set(&rest[1..]),
        _ => {
            eprintln!("arl: usa `arl config get|set`\n\n{CONFIG_USAGE}");
            ExitCode::from(2)
        }
    }
}

fn cmd_config_get(rest: &[String]) -> ExitCode {
    let opts = match parse_opts(rest, CONFIG_USAGE, false) {
        Ok(o) => o,
        Err(code) => return code,
    };
    let key = match opts.positional.as_slice() {
        [] => Value::Null,
        [k] => Value::String(k.clone()),
        _ => {
            eprintln!("arl: config get admite como máximo una clave\n\n{CONFIG_USAGE}");
            return ExitCode::from(2);
        }
    };
    send(&opts.socket, "config.get", &json!({"key": key}))
}

fn cmd_config_set(rest: &[String]) -> ExitCode {
    let opts = match parse_opts(rest, CONFIG_USAGE, true) {
        Ok(o) => o,
        Err(code) => return code,
    };
    let (key, raw) = match opts.positional.as_slice() {
        [k, v] => (k.clone(), v.clone()),
        _ => {
            eprintln!("arl: config set requiere KEY y VALUE\n\n{CONFIG_USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some(expected_revision) = opts.rev else {
        eprintln!("arl: config set requiere --rev N\n\n{CONFIG_USAGE}");
        return ExitCode::from(2);
    };
    let args = json!({
        "key": key,
        "value": parse_value(&raw),
        "expected_revision": expected_revision,
    });
    send(&opts.socket, "config.set", &args)
}

/// Interpreta VALUE de la línea de órdenes: booleano, entero, decimal o texto.
fn parse_value(raw: &str) -> Value {
    match raw {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(i) = raw.parse::<i64>() {
        return json!(i);
    }
    if let Some(f) = raw
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
    {
        return Value::Number(f);
    }
    Value::String(raw.to_owned())
}

/// Envía `op` al daemon, imprime la respuesta y sale con 1 si `ok` no es true.
fn send(path: &Path, op: &str, args: &Value) -> ExitCode {
    match socket::request(path, op, args) {
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
