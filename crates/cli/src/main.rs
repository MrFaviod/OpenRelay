//! `openrelay` — local administration CLI for the openrelay daemon.
//!
//! Usage:
//!
//! ```text
//! openrelay daemon [--socket PATH]            starts the control server in the foreground
//! openrelay status [--socket PATH]            asks the daemon for its state and prints it (JSON)
//! openrelay config get [KEY] [--socket PATH]  reads one key or the full snapshot
//! openrelay config set KEY VALUE --rev N      writes one key with the expected revision
//! openrelay capabilities [--socket PATH]      asks for capabilities and the current revision
//! openrelay pair [--ttl 120] [--advertise HOST] [--yes]  opens a window and approves a phone
//! openrelay devices                           lists the approved identities
//! openrelay devices remove FINGERPRINT        revokes a device and closes its sessions
//! openrelay service print-unit                prints the systemd --user unit
//! ```
//!
//! Exit codes: 0 success; 1 the daemon does not respond or returns an error;
//! 2 incorrect usage.

mod socket;

use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use openrelay_daemon::runtime::{ProvisionFiles, RuntimeConfig};
use serde_json::{json, Value};

const USAGE: &str = "\
openrelay — local administration CLI for the openrelay daemon

Usage:
  openrelay daemon [--socket PATH]            starts the control server (foreground)
  openrelay status [--socket PATH]            shows the daemon state (JSON)
  openrelay config get [KEY] [--socket PATH]  reads one key or all of them (JSON)
  openrelay config set KEY VALUE --rev N      writes one key (JSON)
  openrelay capabilities [--socket PATH]      capabilities and current revision (JSON)
  openrelay pair [--ttl 120] [--advertise HOST] [--yes]
                                              QR + local approval of a phone
  openrelay devices                           lists the approved identities
  openrelay devices grant FINGERPRINT return  allows Linux→Android return audio
  openrelay devices remove FINGERPRINT        revokes a device and closes its sessions
  openrelay service print-unit                prints the systemd --user unit
  openrelay --help | openrelay <subcommand> --help

Default socket: $XDG_RUNTIME_DIR/openrelay/control.sock (0700 dir, 0600 socket).
";

const DAEMON_USAGE: &str = "\
Usage: openrelay daemon [--socket PATH] [--state-dir PATH] [--no-quic]
                 [--quic ADDR | --tailscale]
                 [--identity-der PATH --identity-key PATH]
                 [--client-ca PATH --client-ca-key PATH]
                 [--identities PATH --control-grants PATH]

Starts the control server in the foreground. It only accepts connections from
the same UID. Ctrl-C stops it.

By default it provisions ${XDG_CONFIG_HOME:-~/.config}/openrelay (local CA,
server certificate, empty identities and grants) and opens QUIC on
0.0.0.0:47000. --state-dir changes the directory; the six files override it.
--no-quic leaves the session endpoint off.
--tailscale binds QUIC and pairing to this PC's Tailscale IPv4 (from
`tailscale ip -4`) instead of every interface, and the pairing QR advertises
it: the phone reaches the PC over the tailnet and no LAN port is opened.
";

const PAIR_USAGE: &str = "\
Usage: openrelay pair [--ttl 120] [--advertise HOST] [--yes] [--socket PATH]

Opens a pairing window, prints the URI and the QR, waits for the phone to send
its token and asks for local approval. With --yes it approves the first device
without asking. TTL in seconds (1..=300).
";

const DEVICES_USAGE: &str = "\
Usage: openrelay devices [--state-dir PATH]
     openrelay devices grant FINGERPRINT return [--state-dir PATH]
     openrelay devices remove FINGERPRINT [--socket PATH]

With no arguments it lists the approved identities in identities.json (spki and
capabilities). `grant ... return` turns on the `return_audio` capability for an
already approved device; the daemon reads identities.json at startup, so restart
it for a live session to pick the flag up. `remove` asks the daemon to delete
the identity, take it out of the live ACL and close that device's open sessions.
";

const STATUS_USAGE: &str = "\
Usage: openrelay status [--socket PATH]

Prints the daemon's full JSON response. Exits with 1 if it does not respond.
";

const CONFIG_USAGE: &str = "\
Usage:
  openrelay config get [KEY] [--socket PATH]
  openrelay config set KEY VALUE --rev N [--socket PATH]

get without KEY returns the full snapshot. set requires --rev with the revision
you read before; if it does not match, it exits with 1 and returns a `conflict`
error with `current_revision`. VALUE is interpreted as true/false, integer,
decimal or text. Repeating the same request_id returns the same response
without applying it twice.
";

const CAPABILITIES_USAGE: &str = "\
Usage: openrelay capabilities [--socket PATH]

Prints the daemon's `capabilities` message (revision, keys and limits).
";

const SERVICE_USAGE: &str = "\
Usage: openrelay service print-unit

Prints, without installing it, the contents of packaging/systemd/openrelay.service.
The unit is optional: run `openrelay daemon` under systemd --user.
";

/// Packaged unit. `include_str!` avoids duplicating the file: what is printed
/// is exactly what ships.
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
            eprintln!("openrelay: unknown subcommand: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Common options already split from the positional arguments.
struct Opts {
    socket: PathBuf,
    rev: Option<u64>,
    positional: Vec<String>,
}

/// Parses `[--socket PATH]`, `--rev N` (only if `rev_allowed`), positionals and
/// `--help`. `Err(code)` when the process should exit already.
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
                    eprintln!("openrelay: --socket requires a path");
                    return Err(ExitCode::from(2));
                }
            },
            "--rev" if rev_allowed => match it.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => rev = Some(n),
                None => {
                    eprintln!("openrelay: --rev requires a non-negative integer");
                    return Err(ExitCode::from(2));
                }
            },
            other if other.starts_with('-') && other.len() > 1 && other.parse::<f64>().is_err() => {
                eprintln!("openrelay: unknown argument: {other}\n\n{usage}");
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
                eprintln!("openrelay: set --socket or XDG_RUNTIME_DIR");
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

/// Options with no positionals and no `--rev` (daemon, status, capabilities).
fn parse_plain(rest: &[String], usage: &str) -> Result<PathBuf, ExitCode> {
    let opts = parse_opts(rest, usage, false)?;
    if !opts.positional.is_empty() {
        eprintln!(
            "openrelay: unknown argument: {}\n\n{usage}",
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
            "openrelay: QUIC on {addr} pin {}",
            ready.server_pin.as_deref().unwrap_or("-")
        );
    }
    eprintln!("openrelay: daemon listening on {}", path.display());
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

/// Arguments specific to `openrelay daemon`.
struct DaemonOpts {
    quic: Option<SocketAddr>,
    tailscale: bool,
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
    let mut tailscale = false;
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
                    eprintln!("openrelay: {name} requires a path");
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
            "--tailscale" => tailscale = true,
            "--quic" => match it.next().and_then(|text| text.parse::<SocketAddr>().ok()) {
                Some(addr) => quic = Some(addr),
                None => {
                    eprintln!("openrelay: --quic requires host:port");
                    return Err(ExitCode::from(2));
                }
            },
            other => {
                eprintln!("openrelay: unknown argument: {other}\n\n{DAEMON_USAGE}");
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
                eprintln!("openrelay: set --socket or XDG_RUNTIME_DIR");
                return Err(ExitCode::from(2));
            }
        },
    };
    let files = DaemonOpts {
        quic,
        tailscale,
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
            eprintln!("openrelay: --no-quic and --quic are mutually exclusive\n\n{DAEMON_USAGE}");
            return Err(ExitCode::from(2));
        }
        if self.tailscale && (self.no_quic || self.quic.is_some()) {
            eprintln!("openrelay: --tailscale excludes --quic and --no-quic\n\n{DAEMON_USAGE}");
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
                        "openrelay: provisioning takes all six files or none\n\n{DAEMON_USAGE}"
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
                        eprintln!("openrelay: set --state-dir or HOME/XDG_CONFIG_HOME");
                        return Err(ExitCode::from(2));
                    }
                },
            };
            match openrelay_daemon::provision::ensure(&dir) {
                Ok(files) => Some(files),
                Err(error) => {
                    eprintln!("openrelay: provision ({}): {error}", dir.display());
                    return Err(ExitCode::from(1));
                }
            }
        };
        let quic_bind = if self.no_quic {
            None
        } else {
            match self.quic {
                Some(addr) => Some(addr),
                None if self.tailscale => match tailscale_ipv4() {
                    Ok(ip) => {
                        eprintln!("openrelay: Tailscale address {ip}");
                        Some(SocketAddr::from((ip, 47000)))
                    }
                    Err(error) => {
                        eprintln!("openrelay: --tailscale: {error}");
                        return Err(ExitCode::from(1));
                    }
                },
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

/// This PC's Tailscale IPv4, as reported by `tailscale ip -4`.
fn tailscale_ipv4() -> Result<std::net::Ipv4Addr, String> {
    let output = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .map_err(|error| format!("cannot run `tailscale ip -4`: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`tailscale ip -4` failed: {}", reason.trim()));
    }
    text.lines()
        .find_map(|line| line.trim().parse().ok())
        .ok_or_else(|| "Tailscale reported no IPv4; is it up (`tailscale up`)?".to_owned())
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

/// `openrelay pair`: enrollment window, QR and local approval.
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
                    eprintln!("openrelay: --socket requires a path");
                    return ExitCode::from(2);
                }
            },
            "--ttl" => match it.next().and_then(|n| n.parse::<u64>().ok()) {
                Some(n) => ttl = n,
                None => {
                    eprintln!("openrelay: --ttl requires seconds");
                    return ExitCode::from(2);
                }
            },
            "--advertise" => match it.next() {
                Some(host) => advertise = Some(host.clone()),
                None => {
                    eprintln!("openrelay: --advertise requires a host");
                    return ExitCode::from(2);
                }
            },
            "--yes" => yes = true,
            other => {
                eprintln!("openrelay: unknown argument: {other}\n\n{PAIR_USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(socket) = socket else {
        eprintln!("openrelay: set --socket or XDG_RUNTIME_DIR");
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
    // Close the window on every orderly exit (EOF, rejection, unreadable QR,
    // polling error). Ctrl-C does not run `Drop`: for that, `pair.open` carries
    // `owner_pid` and the daemon retires the window when the process disappears.
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
    eprintln!("openrelay: waiting for the phone; expires at {expires_at} (epoch)");
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
            println!("Device: {fingerprint}");
            let approved = yes || confirm("Approve this device? [y/N] ");
            let (op, extra) = if approved {
                ("pair.approve", json!({ "fingerprint_hex": fingerprint }))
            } else {
                ("pair.reject", json!({ "fingerprint_hex": fingerprint }))
            };
            // Approved and accepted: the window stays open a few seconds so the
            // phone receives its sheet over QUIC. Rejected or error: it closes on
            // exit. It is only disarmed if the daemon confirmed the approval.
            let result = call(&socket, op, &extra);
            if approved && result.is_ok() {
                window.keep_after_approve(true);
            }
            return match result {
                Ok(_) if approved => {
                    println!("openrelay: device approved");
                    ExitCode::SUCCESS
                }
                Ok(_) => {
                    println!("openrelay: device rejected");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("openrelay: {error}");
                    ExitCode::from(1)
                }
            };
        }
        if std::time::Instant::now() >= deadline {
            eprintln!("openrelay: the window expired with no requests");
            return ExitCode::from(1);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// `pair.open` window as seen by the CLI: it closes when dropped.
///
/// It covers every orderly exit, including an early `return` on error. There is
/// no `Drop` for SIGINT/SIGTERM; that half is handled by the daemon with the
/// `owner_pid` that travels in `pair.open`.
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

    /// After approving, the daemon keeps the window for a few seconds so the
    /// phone receives its sheet over QUIC: the CLI does not close it.
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

/// `openrelay devices`: lists `identities.json` without talking to the daemon.
fn cmd_devices(rest: &[String]) -> ExitCode {
    if rest.first().map(String::as_str) == Some("remove") {
        return cmd_devices_remove(&rest[1..]);
    }
    if rest.first().map(String::as_str) == Some("grant") {
        return cmd_devices_grant(&rest[1..]);
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
                    eprintln!("openrelay: --state-dir requires a path");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("openrelay: unknown argument: {other}\n\n{DEVICES_USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let dir = match state_dir.or_else(openrelay_daemon::provision::default_dir) {
        Some(dir) => dir,
        None => {
            eprintln!("openrelay: set --state-dir or HOME/XDG_CONFIG_HOME");
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
        eprintln!("openrelay: no approved devices");
        return ExitCode::SUCCESS;
    }
    for entry in entries {
        let pin = entry["spki"].as_str().unwrap_or("?");
        let flag = |name: &str| {
            if entry[name].as_bool().unwrap_or(false) {
                "yes"
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

/// `openrelay devices grant`: turns on `return_audio` in `identities.json`.
fn cmd_devices_grant(rest: &[String]) -> ExitCode {
    let mut state_dir: Option<PathBuf> = None;
    let mut positional: Vec<&str> = Vec::new();
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
                    eprintln!("openrelay: --state-dir requires a path");
                    return ExitCode::from(2);
                }
            },
            other if other.starts_with('-') => {
                eprintln!("openrelay: unknown argument: {other}\n\n{DEVICES_USAGE}");
                return ExitCode::from(2);
            }
            other => positional.push(other),
        }
    }
    let (fingerprint, capability) = match positional.as_slice() {
        [fingerprint, "return"] => (*fingerprint, "return"),
        [_, other] => {
            eprintln!("openrelay: unknown capability: {other} (use `return`)\n\n{DEVICES_USAGE}");
            return ExitCode::from(2);
        }
        _ => {
            eprintln!(
                "openrelay: devices grant requires FINGERPRINT and `return`\n\n{DEVICES_USAGE}"
            );
            return ExitCode::from(2);
        }
    };
    let dir = match state_dir.or_else(openrelay_daemon::provision::default_dir) {
        Some(dir) => dir,
        None => {
            eprintln!("openrelay: set --state-dir or HOME/XDG_CONFIG_HOME");
            return ExitCode::from(2);
        }
    };
    let path = dir.join("identities.json");
    match openrelay_daemon::pairing::grant_return_audio(&path, fingerprint) {
        Ok(true) => {
            println!("openrelay: {capability} granted to {fingerprint}");
            println!("openrelay: restart the daemon so the live ACL reloads it");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("openrelay: {capability} was already granted to {fingerprint}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("openrelay: {}: {error}", path.display());
            ExitCode::from(1)
        }
    }
}

/// `openrelay devices remove`: asks the daemon to revoke an identity.
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
                    eprintln!("openrelay: --socket requires a path");
                    return ExitCode::from(2);
                }
            },
            other if other.starts_with('-') => {
                eprintln!("openrelay: unknown argument: {other}\n\n{DEVICES_USAGE}");
                return ExitCode::from(2);
            }
            other => {
                if fingerprint.replace(other.to_string()).is_some() {
                    eprintln!("openrelay: devices remove accepts a single fingerprint");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let Some(fingerprint) = fingerprint else {
        eprintln!("openrelay: devices remove requires a fingerprint\n\n{DEVICES_USAGE}");
        return ExitCode::from(2);
    };
    let Some(socket) = socket.or_else(default_socket) else {
        eprintln!("openrelay: set --socket or XDG_RUNTIME_DIR");
        return ExitCode::from(2);
    };
    match call(
        &socket,
        "devices.remove",
        &json!({ "fingerprint_hex": fingerprint }),
    ) {
        Ok(result) => {
            let hex = result["fingerprint_hex"].as_str().unwrap_or(&fingerprint);
            println!("openrelay: device removed {hex}");
            println!(
                "sessions closed: {}",
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

/// Default control socket from `XDG_RUNTIME_DIR`.
fn default_socket() -> Option<PathBuf> {
    match env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => {
            Some(PathBuf::from(dir).join("openrelay").join("control.sock"))
        }
        _ => None,
    }
}

/// Sends an operation and returns `result`, or the daemon error as text.
fn call(path: &Path, op: &str, args: &Value) -> Result<Value, String> {
    let resp = socket::request(path, op, args)?;
    if resp.get("ok") == Some(&Value::Bool(true)) {
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    } else {
        let code = resp["error"]["code"].as_str().unwrap_or("error");
        let message = resp["error"]["message"].as_str().unwrap_or("no detail");
        Err(format!("{code}: {message}"))
    }
}

/// Asks yes/no on stdin; anything other than `y`/`s` counts as no.
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

/// `openrelay service print-unit`: dumps the unit without writing or installing anything.
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
            eprintln!("openrelay: use `openrelay service print-unit`\n\n{SERVICE_USAGE}");
            ExitCode::from(2)
        }
        [other, ..] => {
            eprintln!("openrelay: unknown service subcommand: {other}\n\n{SERVICE_USAGE}");
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
            eprintln!("openrelay: use `openrelay config get|set`\n\n{CONFIG_USAGE}");
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
            eprintln!("openrelay: config get accepts at most one key\n\n{CONFIG_USAGE}");
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
            eprintln!("openrelay: config set requires KEY and VALUE\n\n{CONFIG_USAGE}");
            return ExitCode::from(2);
        }
    };
    let Some(expected_revision) = opts.rev else {
        eprintln!("openrelay: config set requires --rev N\n\n{CONFIG_USAGE}");
        return ExitCode::from(2);
    };
    let args = json!({
        "key": key,
        "value": parse_value(&raw),
        "expected_revision": expected_revision,
    });
    send(&opts.socket, "config.set", &args)
}

/// Interprets VALUE from the command line: boolean, integer, decimal or text.
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

/// Sends `op` to the daemon, prints the response and exits with 1 if `ok` is not true.
fn send(path: &Path, op: &str, args: &Value) -> ExitCode {
    match socket::request(path, op, args) {
        Ok(resp) => {
            println!("{resp}");
            if resp.get("ok") == Some(&Value::Bool(true)) {
                ExitCode::SUCCESS
            } else {
                eprintln!("openrelay: the daemon returned an error");
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("openrelay: {e}");
            ExitCode::from(1)
        }
    }
}
