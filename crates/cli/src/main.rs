//! `openrelay` — CLI local de administración del daemon de openrelay.
//!
//! Uso:
//!
//! ```text
//! openrelay daemon [--socket PATH]            arranca el servidor de control en primer plano
//! openrelay status [--socket PATH]            pide el estado al daemon y lo imprime (JSON)
//! openrelay config get [KEY] [--socket PATH]  lee una clave o el snapshot completo
//! openrelay config set KEY VALUE --rev N      escribe una clave con revisión esperada
//! openrelay capabilities [--socket PATH]      pide las capacidades y la revisión actual
//! openrelay pair [--ttl 120] [--advertise HOST] [--yes]  abre una ventana y aprueba un teléfono
//! openrelay devices                           lista las identidades aprobadas
//! openrelay devices remove FINGERPRINT        revoca un dispositivo y cierra sus sesiones
//! openrelay service print-unit                imprime la unidad systemd --user
//! ```
//!
//! Códigos de salida: 0 correcto; 1 el daemon no responde o devuelve error;
//! 2 uso incorrecto.

mod socket;

use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use openrelay_daemon::runtime::{ProvisionFiles, RuntimeConfig};
use serde_json::{json, Value};

const USAGE: &str = "\
openrelay — CLI de administración del daemon de openrelay

Uso:
  openrelay daemon [--socket PATH]            arranca el servidor de control (primer plano)
  openrelay status [--socket PATH]            muestra el estado del daemon (JSON)
  openrelay config get [KEY] [--socket PATH]  lee una clave o todas (JSON)
  openrelay config set KEY VALUE --rev N      escribe una clave (JSON)
  openrelay capabilities [--socket PATH]      capacidades y revisión actual (JSON)
  openrelay pair [--ttl 120] [--advertise HOST] [--yes]
                                              QR + aprobación local de un teléfono
  openrelay devices                           lista las identidades aprobadas
  openrelay devices remove FINGERPRINT       revoca un dispositivo y cierra sus sesiones
  openrelay service print-unit                imprime la unidad systemd --user
  openrelay --help | openrelay <subcomando> --help

Socket por defecto: $XDG_RUNTIME_DIR/openrelay/control.sock (0700 dir, 0600 socket).
";

const DAEMON_USAGE: &str = "\
Uso: openrelay daemon [--socket PATH] [--state-dir PATH] [--no-quic]
                 [--quic ADDR]
                 [--identity-der PATH --identity-key PATH]
                 [--client-ca PATH --client-ca-key PATH]
                 [--identities PATH --control-grants PATH]

Arranca el servidor de control en primer plano. Sólo acepta conexiones del
mismo UID. Ctrl-C para detenerlo.

Por defecto aprovisiona ${XDG_CONFIG_HOME:-~/.config}/openrelay (CA local,
certificado del servidor, identidades vacías y grants) y abre QUIC en
0.0.0.0:47000. --state-dir cambia el directorio; los seis archivos lo
sobreescriben. --no-quic deja el endpoint de sesión apagado.
";

const PAIR_USAGE: &str = "\
Uso: openrelay pair [--ttl 120] [--advertise HOST] [--yes] [--socket PATH]

Abre una ventana de emparejamiento, imprime el URI y el QR, espera a que el
teléfono envíe su token y pide aprobación local. Con --yes aprueba el primer
dispositivo sin preguntar. TTL en segundos (1..=300).
";

const DEVICES_USAGE: &str = "\
Uso: openrelay devices [--state-dir PATH]
     openrelay devices remove FINGERPRINT [--socket PATH]

Sin argumentos lista las identidades aprobadas en identities.json (spki y
capacidades). `remove` pide al daemon que borre la identidad, la saque de la
ACL viva y cierre las sesiones abiertas de ese dispositivo.
";

const STATUS_USAGE: &str = "\
Uso: openrelay status [--socket PATH]

Imprime la respuesta JSON completa del daemon. Sale con 1 si no responde.
";

const CONFIG_USAGE: &str = "\
Uso:
  openrelay config get [KEY] [--socket PATH]
  openrelay config set KEY VALUE --rev N [--socket PATH]

get sin KEY devuelve el snapshot completo. set exige --rev con la revisión
que leíste antes; si no coincide, sale con 1 y devuelve error `conflict` con
`current_revision`. VALUE se interpreta como true/false, entero, decimal o
texto. Un mismo request_id repetido devuelve la misma respuesta sin aplicar
dos veces.
";

const CAPABILITIES_USAGE: &str = "\
Uso: openrelay capabilities [--socket PATH]

Imprime el mensaje `capabilities` del daemon (revisión, claves y límites).
";

const SERVICE_USAGE: &str = "\
Uso: openrelay service print-unit

Imprime, sin instalarlo, el contenido de packaging/systemd/openrelay.service.
La unidad es opcional: corre `openrelay daemon` bajo systemd --user.
";

/// Unidad empaquetada. `include_str!` evita duplicar el archivo: lo que se
/// imprime es exactamente lo que se distribuye.
const SERVICE_UNIT: &str = include_str!("../../../packaging/systemd/openrelay.service");

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
        "pair" => cmd_pair(rest),
        "devices" => cmd_devices(rest),
        "service" => cmd_service(rest),
        other => {
            eprintln!("openrelay: subcomando desconocido: {other}\n\n{USAGE}");
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
                    eprintln!("openrelay: --socket requiere una ruta");
                    return Err(ExitCode::from(2));
                }
            },
            "--rev" if rev_allowed => match it.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => rev = Some(n),
                None => {
                    eprintln!("openrelay: --rev requiere un entero no negativo");
                    return Err(ExitCode::from(2));
                }
            },
            other if other.starts_with('-') && other.len() > 1 && other.parse::<f64>().is_err() => {
                eprintln!("openrelay: argumento desconocido: {other}\n\n{usage}");
                return Err(ExitCode::from(2));
            }
            other => positional.push(other.to_owned()),
        }
    }
    let socket = match socket {
        Some(p) => p,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => {
                PathBuf::from(dir).join("openrelay").join("control.sock")
            }
            _ => {
                eprintln!("openrelay: define --socket o XDG_RUNTIME_DIR");
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
            "openrelay: argumento desconocido: {}\n\n{usage}",
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
    let (mut runtime, ready) = match openrelay_daemon::runtime::start(&config) {
        Ok(started) => started,
        Err(error) => {
            eprintln!("openrelay: daemon: {error}");
            return ExitCode::from(1);
        }
    };
    if let Some(gap) = &ready.quic_gap {
        eprintln!("openrelay: {gap}");
    } else if let Some(addr) = ready.quic_addr {
        eprintln!(
            "openrelay: QUIC en {addr} pin {}",
            ready.server_pin.as_deref().unwrap_or("-")
        );
    }
    eprintln!("openrelay: daemon escuchando en {}", path.display());
    let served = socket::serve_interleaved(&path, |event| match event {
        socket::Interleave::Request {
            op,
            request_id,
            args,
        } => runtime.local_op(op, request_id, args),
        socket::Interleave::Idle => {
            if let Err(error) = runtime.pump_quic() {
                eprintln!("openrelay: quic: {error}");
            }
            Ok(Value::Null)
        }
    });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("openrelay: daemon: {error}");
            ExitCode::from(1)
        }
    }
}

/// Argumentos propios de `openrelay daemon`.
struct DaemonOpts {
    quic: Option<SocketAddr>,
    no_quic: bool,
    state_dir: Option<PathBuf>,
    identity_der: Option<PathBuf>,
    identity_key: Option<PathBuf>,
    client_ca: Option<PathBuf>,
    client_ca_key: Option<PathBuf>,
    identities: Option<PathBuf>,
    control_grants: Option<PathBuf>,
}

fn parse_daemon(rest: &[String]) -> Result<(PathBuf, RuntimeConfig), ExitCode> {
    let mut socket: Option<PathBuf> = None;
    let mut quic: Option<SocketAddr> = None;
    let mut no_quic = false;
    let mut state_dir: Option<PathBuf> = None;
    let mut identity_der: Option<PathBuf> = None;
    let mut identity_key: Option<PathBuf> = None;
    let mut client_ca: Option<PathBuf> = None;
    let mut client_ca_key: Option<PathBuf> = None;
    let mut identities: Option<PathBuf> = None;
    let mut control_grants: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        let take = |name: &str, it: &mut std::slice::Iter<String>| -> Result<PathBuf, ExitCode> {
            match it.next() {
                Some(path) => Ok(PathBuf::from(path)),
                None => {
                    eprintln!("openrelay: {name} requiere una ruta");
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
            "--state-dir" => state_dir = Some(take("--state-dir", &mut it)?),
            "--identity-der" => identity_der = Some(take("--identity-der", &mut it)?),
            "--identity-key" => identity_key = Some(take("--identity-key", &mut it)?),
            "--client-ca" => client_ca = Some(take("--client-ca", &mut it)?),
            "--client-ca-key" => client_ca_key = Some(take("--client-ca-key", &mut it)?),
            "--identities" => identities = Some(take("--identities", &mut it)?),
            "--control-grants" => control_grants = Some(take("--control-grants", &mut it)?),
            "--no-quic" => no_quic = true,
            "--quic" => match it.next().and_then(|text| text.parse::<SocketAddr>().ok()) {
                Some(addr) => quic = Some(addr),
                None => {
                    eprintln!("openrelay: --quic requiere host:puerto");
                    return Err(ExitCode::from(2));
                }
            },
            other => {
                eprintln!("openrelay: argumento desconocido: {other}\n\n{DAEMON_USAGE}");
                return Err(ExitCode::from(2));
            }
        }
    }
    let socket = match socket {
        Some(path) => path,
        None => match env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => {
                PathBuf::from(dir).join("openrelay").join("control.sock")
            }
            _ => {
                eprintln!("openrelay: define --socket o XDG_RUNTIME_DIR");
                return Err(ExitCode::from(2));
            }
        },
    };
    let files = DaemonOpts {
        quic,
        no_quic,
        state_dir,
        identity_der,
        identity_key,
        client_ca,
        client_ca_key,
        identities,
        control_grants,
    };
    Ok((socket, files.into_config()?))
}

impl DaemonOpts {
    fn into_config(self) -> Result<RuntimeConfig, ExitCode> {
        if self.no_quic && self.quic.is_some() {
            eprintln!("openrelay: --no-quic y --quic son excluyentes\n\n{DAEMON_USAGE}");
            return Err(ExitCode::from(2));
        }
        let any_file = self.identity_der.is_some()
            || self.identity_key.is_some()
            || self.client_ca.is_some()
            || self.client_ca_key.is_some()
            || self.identities.is_some()
            || self.control_grants.is_some();
        let provision = if any_file {
            match (
                self.identity_der,
                self.identity_key,
                self.client_ca,
                self.client_ca_key,
                self.identities,
                self.control_grants,
            ) {
                (
                    Some(identity_der),
                    Some(identity_key),
                    Some(client_ca),
                    Some(client_ca_key),
                    Some(identities),
                    Some(control_grants),
                ) => Some(ProvisionFiles {
                    identity_der,
                    identity_key,
                    client_ca_der: client_ca,
                    client_ca_key,
                    identities,
                    control_grants,
                }),
                _ => {
                    eprintln!(
                        "openrelay: la provisión pide los seis archivos o ninguno\n\n{DAEMON_USAGE}"
                    );
                    return Err(ExitCode::from(2));
                }
            }
        } else if self.no_quic {
            None
        } else {
            let dir = match self.state_dir {
                Some(dir) => dir,
                None => match openrelay_daemon::provision::default_dir() {
                    Some(dir) => dir,
                    None => {
                        eprintln!("openrelay: define --state-dir o HOME/XDG_CONFIG_HOME");
                        return Err(ExitCode::from(2));
                    }
                },
            };
            match openrelay_daemon::provision::ensure(&dir) {
                Ok(files) => Some(files),
                Err(error) => {
                    eprintln!("openrelay: provisión ({}): {error}", dir.display());
                    return Err(ExitCode::from(1));
                }
            }
        };
        let quic_bind = if self.no_quic {
            None
        } else {
            match self.quic {
                Some(addr) => Some(addr),
                None => Some(SocketAddr::from(([0, 0, 0, 0], 47000))),
            }
        };
        Ok(RuntimeConfig {
            quic_bind,
            provision,
            local_device_id: "openrelay-cli-local".to_owned(),
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

/// `openrelay pair`: ventana de enrolamiento, QR y aprobación local.
fn cmd_pair(rest: &[String]) -> ExitCode {
    let mut socket = default_socket();
    let mut ttl = 120u64;
    let mut advertise: Option<String> = None;
    let mut yes = false;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{PAIR_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--socket" => match it.next() {
                Some(path) => socket = Some(PathBuf::from(path)),
                None => {
                    eprintln!("openrelay: --socket requiere una ruta");
                    return ExitCode::from(2);
                }
            },
            "--ttl" => match it.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => ttl = n,
                None => {
                    eprintln!("openrelay: --ttl requiere segundos");
                    return ExitCode::from(2);
                }
            },
            "--advertise" => match it.next() {
                Some(host) => advertise = Some(host.clone()),
                None => {
                    eprintln!("openrelay: --advertise requiere un host");
                    return ExitCode::from(2);
                }
            },
            "--yes" => yes = true,
            other => {
                eprintln!("openrelay: argumento desconocido: {other}\n\n{PAIR_USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("openrelay: define --socket o XDG_RUNTIME_DIR");
        return ExitCode::from(2);
    };
    let mut args = json!({ "ttl_s": ttl, "owner_pid": std::process::id() });
    if let Some(host) = advertise {
        args["advertise"] = Value::String(host);
    }
    let opened = match call(&socket, "pair.open", &args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("openrelay: {error}");
            return ExitCode::from(1);
        }
    };
    // Cierra la ventana en toda salida ordenada (EOF, rechazo, QR ilegible,
    // error de sondeo). Ctrl-C no ejecuta `Drop`: para eso `pair.open` lleva
    // `owner_pid` y el daemon retira la ventana cuando el proceso desaparece.
    let mut window = PairWindow::new(&socket);
    let uri = opened["uri"].as_str().unwrap_or_default();
    let expires_at = opened["expires_at"].as_u64().unwrap_or(0);
    println!("{uri}");
    match qrcode::QrCode::new(uri.as_bytes()) {
        Ok(code) => {
            let qr = code.render::<qrcode::render::unicode::Dense1x2>().build();
            println!("{qr}");
        }
        Err(error) => {
            eprintln!("openrelay: QR: {error}");
            return ExitCode::from(1);
        }
    }
    eprintln!("openrelay: esperando el teléfono; expira en {expires_at} (epoch)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ttl + 5);
    loop {
        let pending = match call(&socket, "pair.pending", &Value::Null) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("openrelay: {error}");
                return ExitCode::from(1);
            }
        };
        if let Some(first) = pending.as_array().and_then(|items| items.first()) {
            let fingerprint = first["fingerprint_hex"].as_str().unwrap_or_default();
            println!("Dispositivo: {fingerprint}");
            let approved = yes || confirm("¿Aprobar este dispositivo? [y/N] ");
            let (op, extra) = if approved {
                ("pair.approve", json!({ "fingerprint_hex": fingerprint }))
            } else {
                ("pair.reject", json!({ "fingerprint_hex": fingerprint }))
            };
            // Aprobado y aceptado: la ventana sigue unos segundos para que el
            // teléfono reciba su hoja por QUIC. Rechazado o error: se cierra al
            // salir. Sólo se desarma si el daemon confirmó la aprobación.
            let result = call(&socket, op, &extra);
            if approved && result.is_ok() {
                window.keep_after_approve(true);
            }
            return match result {
                Ok(_) if approved => {
                    println!("openrelay: dispositivo aprobado");
                    ExitCode::SUCCESS
                }
                Ok(_) => {
                    println!("openrelay: dispositivo rechazado");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("openrelay: {error}");
                    ExitCode::from(1)
                }
            };
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("openrelay: la ventana expiró sin solicitudes");
            return ExitCode::from(1);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Ventana de `pair.open` vista por el CLI: se cierra al soltarla.
///
/// Cubre toda salida ordenada, incluido un `return` temprano por error. Para
/// SIGINT/SIGTERM no hay `Drop`; esa mitad la resuelve el daemon con el
/// `owner_pid` que viaja en `pair.open`.
struct PairWindow<'a> {
    socket: &'a Path,
    armed: bool,
}

impl<'a> PairWindow<'a> {
    fn new(socket: &'a Path) -> Self {
        Self {
            socket,
            armed: true,
        }
    }

    /// Tras aprobar, el daemon conserva la ventana unos segundos para que el
    /// teléfono reciba su hoja por QUIC: el CLI no la cierra.
    fn keep_after_approve(&mut self, approved: bool) {
        self.armed = !approved;
    }
}

impl Drop for PairWindow<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = call(self.socket, "pair.close", &Value::Null);
        }
    }
}

/// `openrelay devices`: lista `identities.json` sin hablar con el daemon.
fn cmd_devices(rest: &[String]) -> ExitCode {
    if rest.first().map(String::as_str) == Some("remove") {
        return cmd_devices_remove(&rest[1..]);
    }
    let mut state_dir: Option<PathBuf> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{DEVICES_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--state-dir" => match it.next() {
                Some(path) => state_dir = Some(PathBuf::from(path)),
                None => {
                    eprintln!("openrelay: --state-dir requiere una ruta");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("openrelay: argumento desconocido: {other}\n\n{DEVICES_USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let dir = match state_dir.or_else(openrelay_daemon::provision::default_dir) {
        Some(dir) => dir,
        None => {
            eprintln!("openrelay: define --state-dir o HOME/XDG_CONFIG_HOME");
            return ExitCode::from(2);
        }
    };
    let path = dir.join("identities.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("openrelay: {}: {error}", path.display());
            return ExitCode::from(1);
        }
    };
    let doc: Value = match serde_json::from_str(&text) {
        Ok(doc) => doc,
        Err(error) => {
            eprintln!("openrelay: {}: {error}", path.display());
            return ExitCode::from(1);
        }
    };
    let entries = doc["identities"].as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        eprintln!("openrelay: no hay dispositivos aprobados");
        return ExitCode::SUCCESS;
    }
    for entry in entries {
        let pin = entry["spki"].as_str().unwrap_or("?");
        let flag = |name: &str| {
            if entry[name].as_bool().unwrap_or(false) {
                "sí"
            } else {
                "no"
            }
        };
        println!(
            "{pin}  mic={} control_read={} control_write={} return_audio={}",
            flag("mic"),
            flag("control_read"),
            flag("control_write"),
            flag("return_audio")
        );
    }
    ExitCode::SUCCESS
}

/// `openrelay devices remove`: pide al daemon que revoque una identidad.
fn cmd_devices_remove(rest: &[String]) -> ExitCode {
    let mut socket: Option<PathBuf> = None;
    let mut fingerprint: Option<String> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{DEVICES_USAGE}");
                return ExitCode::SUCCESS;
            }
            "--socket" => match it.next() {
                Some(path) => socket = Some(PathBuf::from(path)),
                None => {
                    eprintln!("openrelay: --socket requiere una ruta");
                    return ExitCode::from(2);
                }
            },
            other if other.starts_with('-') => {
                eprintln!("openrelay: argumento desconocido: {other}\n\n{DEVICES_USAGE}");
                return ExitCode::from(2);
            }
            other => {
                if fingerprint.replace(other.to_string()).is_some() {
                    eprintln!("openrelay: devices remove acepta un solo fingerprint");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let Some(fingerprint) = fingerprint else {
        eprintln!("openrelay: devices remove requiere un fingerprint\n\n{DEVICES_USAGE}");
        return ExitCode::from(2);
    };
    let Some(socket) = socket.or_else(default_socket) else {
        eprintln!("openrelay: define --socket o XDG_RUNTIME_DIR");
        return ExitCode::from(2);
    };
    match call(
        &socket,
        "devices.remove",
        &json!({ "fingerprint_hex": fingerprint }),
    ) {
        Ok(result) => {
            let hex = result["fingerprint_hex"].as_str().unwrap_or(&fingerprint);
            println!("openrelay: dispositivo eliminado {hex}");
            println!(
                "sesiones cerradas: {}",
                result["closed_sessions"].as_u64().unwrap_or(0)
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("openrelay: {error}");
            ExitCode::from(1)
        }
    }
}

/// Socket de control por defecto a partir de `XDG_RUNTIME_DIR`.
fn default_socket() -> Option<PathBuf> {
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => {
            Some(PathBuf::from(dir).join("openrelay").join("control.sock"))
        }
        _ => None,
    }
}

/// Envía una operación y devuelve `result`, o el error del daemon como texto.
fn call(path: &Path, op: &str, args: &Value) -> Result<Value, String> {
    let resp = socket::request(path, op, args)?;
    if resp.get("ok") == Some(&Value::Bool(true)) {
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    } else {
        let code = resp["error"]["code"].as_str().unwrap_or("error");
        let message = resp["error"]["message"].as_str().unwrap_or("sin detalle");
        Err(format!("{code}: {message}"))
    }
}

/// Pregunta sí/no por stdin; cualquier cosa que no sea `y`/`s` es no.
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "s" | "si" | "sí"
    )
}

/// `openrelay service print-unit`: vuelca la unidad sin escribir ni instalar nada.
fn cmd_service(rest: &[String]) -> ExitCode {
    match rest {
        [arg] if arg == "print-unit" => {
            print!("{SERVICE_UNIT}");
            ExitCode::SUCCESS
        }
        [arg] if arg == "--help" || arg == "-h" => {
            print!("{SERVICE_USAGE}");
            ExitCode::SUCCESS
        }
        [] => {
            eprintln!("openrelay: usa `openrelay service print-unit`\n\n{SERVICE_USAGE}");
            ExitCode::from(2)
        }
        [other, ..] => {
            eprintln!("openrelay: subcomando de service desconocido: {other}\n\n{SERVICE_USAGE}");
            ExitCode::from(2)
        }
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
            eprintln!("openrelay: usa `openrelay config get|set`\n\n{CONFIG_USAGE}");
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
            eprintln!("openrelay: config get admite como máximo una clave\n\n{CONFIG_USAGE}");
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
            eprintln!("openrelay: config set requiere KEY y VALUE\n\n{CONFIG_USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some(expected_revision) = opts.rev else {
        eprintln!("openrelay: config set requiere --rev N\n\n{CONFIG_USAGE}");
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
                eprintln!("openrelay: el daemon devolvió error");
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("openrelay: {e}");
            ExitCode::from(1)
        }
    }
}
