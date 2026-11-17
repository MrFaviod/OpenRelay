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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use arl_daemon::runtime::{ProvisionFiles, RuntimeConfig};
use serde_json::{json, Value};

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
                 [--quic ADDR]
                 [--identity-der PATH --identity-key PATH]
                 [--client-ca PATH --identities PATH --control-grants PATH]

Arranca el servidor de control en primer plano. Sólo acepta conexiones del
mismo UID. Ctrl-C para detenerlo.

Sin --quic el endpoint de sesión queda apagado: no hay identidad de
producción que fabricar. Con --quic hacen falta los cinco archivos, ya
aprovisionados; este comando no genera claves ni certificados.
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
    let (path, config) = match parse_daemon(rest) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let (mut runtime, ready) = match arl_daemon::runtime::start(&config) {
        Ok(started) => started,
        Err(error) => {
            eprintln!("arl: daemon: {error}");
            return ExitCode::from(1);
        }
    };
    if let Some(gap) = &ready.quic_gap {
        eprintln!("arl: {gap}");
    } else if let Some(addr) = ready.quic_addr {
        eprintln!(
            "arl: QUIC en {addr} pin {}",
            ready.server_pin.as_deref().unwrap_or("-")
        );
    }
    eprintln!("arl: daemon escuchando en {}", path.display());
    let served = socket::serve_interleaved(&path, |event| match event {
        socket::Interleave::Request {
            op,
            request_id,
            args,
        } => runtime.local_op(op, request_id, args),
        socket::Interleave::Idle => {
            if let Err(error) = runtime.pump_quic() {
                eprintln!("arl: quic: {error}");
            }
            Ok(Value::Null)
        }
    });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arl: daemon: {error}");
            ExitCode::from(1)
        }
    }
}

/// Argumentos propios de `arl daemon`. El resto lo resuelve `parse_opts`.
struct DaemonOpts {
    quic: Option<SocketAddr>,
    identity_der: Option<PathBuf>,
    identity_key: Option<PathBuf>,
    client_ca: Option<PathBuf>,
    identities: Option<PathBuf>,
    control_grants: Option<PathBuf>,
}

fn parse_daemon(rest: &[String]) -> Result<(PathBuf, RuntimeConfig), ExitCode> {
    let mut socket: Option<PathBuf> = None;
    let mut quic: Option<SocketAddr> = None;
    let mut identity_der: Option<PathBuf> = None;
    let mut identity_key: Option<PathBuf> = None;
    let mut client_ca: Option<PathBuf> = None;
    let mut identities: Option<PathBuf> = None;
    let mut control_grants: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let take = |name: &str, it: &mut std::slice::Iter<String>| -> Result<PathBuf, ExitCode> {
            match it.next() {
                Some(path) => Ok(PathBuf::from(path)),
                None => {
                    eprintln!("arl: {name} requiere una ruta");
                    Err(ExitCode::from(2))
                }
            }
        };
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{DAEMON_USAGE}");
                return Err(ExitCode::SUCCESS);
            }
            "--socket" => socket = Some(take("--socket", &mut it)?),
            "--identity-der" => identity_der = Some(take("--identity-der", &mut it)?),
            "--identity-key" => identity_key = Some(take("--identity-key", &mut it)?),
            "--client-ca" => client_ca = Some(take("--client-ca", &mut it)?),
            "--identities" => identities = Some(take("--identities", &mut it)?),
            "--control-grants" => control_grants = Some(take("--control-grants", &mut it)?),
            "--quic" => match it.next().and_then(|text| text.parse::<SocketAddr>().ok()) {
                Some(addr) => quic = Some(addr),
                None => {
                    eprintln!("arl: --quic requiere host:puerto");
                    return Err(ExitCode::from(2));
                }
            },
            other => {
                eprintln!("arl: argumento desconocido: {other}\n\n{DAEMON_USAGE}");
                return Err(ExitCode::from(2));
            }
        }
    }
    let socket = match socket {
        Some(path) => path,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("arl").join("control.sock"),
            _ => {
                eprintln!("arl: define --socket o XDG_RUNTIME_DIR");
                return Err(ExitCode::from(2));
            }
        },
    };
    let files = DaemonOpts {
        quic,
        identity_der,
        identity_key,
        client_ca,
        identities,
        control_grants,
    };
    Ok((socket, files.into_config()?))
}

impl DaemonOpts {
    fn into_config(self) -> Result<RuntimeConfig, ExitCode> {
        let provision = match (
            self.identity_der,
            self.identity_key,
            self.client_ca,
            self.identities,
            self.control_grants,
        ) {
            (None, None, None, None, None) => None,
            (
                Some(identity_der),
                Some(identity_key),
                Some(client_ca),
                Some(identities),
                Some(control_grants),
            ) => Some(ProvisionFiles {
                identity_der,
                identity_key,
                client_ca_der: client_ca,
                identities,
                control_grants,
            }),
            _ => {
                eprintln!("arl: la provisión pide los cinco archivos o ninguno\n\n{DAEMON_USAGE}");
                return Err(ExitCode::from(2));
            }
        };
        if self.quic.is_some() && provision.is_none() {
            eprintln!("arl: --quic requiere la provisión completa\n\n{DAEMON_USAGE}");
            return Err(ExitCode::from(2));
        }
        if self.quic.is_none() && provision.is_some() {
            eprintln!("arl: la provisión no se usa sin --quic\n\n{DAEMON_USAGE}");
            return Err(ExitCode::from(2));
        }
        Ok(RuntimeConfig {
            quic_bind: self.quic,
            provision,
            local_device_id: "arl-cli-local".to_owned(),
        })
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
