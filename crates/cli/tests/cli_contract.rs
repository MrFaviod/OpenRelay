//! Contrato del binario `arl`: socket local, permisos, respuestas correlacionadas
//! y ausencia de listeners TCP en el proceso del daemon.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

const ARL: &str = env!("CARGO_BIN_EXE_arl");

/// Directorio temporal propio del test; se borra al soltarlo.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("arl-cli-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("crear tmpdir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Daemon en primer plano que se mata al soltarlo.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(sock: &Path) -> Daemon {
    let child = Command::new(ARL)
        .args(["daemon", "--socket"])
        .arg(sock)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("arrancar arl daemon");
    let daemon = Daemon(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < deadline, "el socket no apareció");
        sleep(Duration::from_millis(20));
    }
    daemon
}

/// Envía líneas crudas y devuelve la primera respuesta como JSON.
fn raw_request(sock: &Path, line: &str) -> serde_json::Value {
    let mut s = UnixStream::connect(sock).expect("conectar");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut resp = String::new();
    BufReader::new(s)
        .read_line(&mut resp)
        .expect("leer respuesta");
    serde_json::from_str(resp.trim_end()).expect("respuesta JSON")
}

#[test]
fn socket_permissions_are_0700_dir_and_0600_socket() {
    let tmp = TempDir::new("perm");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let dir_mode = fs::metadata(sock.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let sock_mode = fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(sock_mode, 0o600);
}

#[test]
fn status_exits_zero_with_correlated_json() {
    let tmp = TempDir::new("status");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let out = Command::new(ARL)
        .args(["status", "--socket"])
        .arg(&sock)
        .output()
        .expect("ejecutar arl status");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["sessions"], 0);
    assert!(v["result"]["version"].is_string());
    assert!(v["request_id"].as_str().unwrap().starts_with("cli-"));
}

#[test]
fn malformed_and_unknown_ops_return_errors_and_daemon_survives() {
    let tmp = TempDir::new("errors");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let bad = raw_request(&sock, "esto no es json");
    assert_eq!(bad["ok"], false);
    assert_eq!(bad["error"]["code"], "invalid");

    let unknown = raw_request(&sock, r#"{"request_id":"req-42","op":"nope"}"#);
    assert_eq!(unknown["request_id"], "req-42");
    assert_eq!(unknown["ok"], false);
    assert_eq!(unknown["error"]["code"], "unsupported");

    let ok = raw_request(&sock, r#"{"request_id":"req-43","op":"status"}"#);
    assert_eq!(ok["request_id"], "req-43");
    assert_eq!(ok["ok"], true);
}

#[test]
fn status_fails_nonzero_when_no_daemon() {
    let tmp = TempDir::new("nodaemon");
    let sock = tmp.0.join("arl").join("control.sock");
    let out = Command::new(ARL)
        .args(["status", "--socket"])
        .arg(&sock)
        .output()
        .expect("ejecutar arl status");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn daemon_has_no_tcp_listeners() {
    let tmp = TempDir::new("tcp");
    let sock = tmp.0.join("arl").join("control.sock");
    let d = start_daemon(&sock);
    let pid = d.0.id();

    // Por fd: todo socket abierto por el daemon debe ser unix. (/proc/<pid>/net/tcp
    // es de todo el netns, así que no sirve para atribuir al proceso.)
    let unix_inodes: Vec<String> = fs::read_to_string(format!("/proc/{pid}/net/unix"))
        .expect("leer unix")
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(6).map(str::to_owned))
        .collect();
    let mut sockets = 0;
    for entry in fs::read_dir(format!("/proc/{pid}/fd")).expect("leer fd") {
        let target = fs::read_link(entry.unwrap().path()).unwrap();
        let t = target.to_string_lossy();
        if let Some(inode) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
            assert!(
                unix_inodes.iter().any(|u| u == inode),
                "socket no unix: {t}"
            );
            sockets += 1;
        }
    }
    assert!(sockets >= 1, "el daemon debe tener al menos su socket unix");
}

/// Clave compartida real del catálogo (`audio.gain_db`: número, paso 0.5).
const KEY: &str = "audio.gain_db";

fn set_line(id: &str, rev: u64, value: &str) -> String {
    format!(
        r#"{{"request_id":"{id}","op":"config.set","args":{{"key":"{KEY}","value":{value},"expected_revision":{rev}}}}}"#
    )
}

fn get_line(id: &str, key: Option<&str>) -> String {
    let key = key.map_or("null".to_owned(), |k| format!("\"{k}\""));
    format!(r#"{{"request_id":"{id}","op":"config.get","args":{{"key":{key}}}}}"#)
}

#[test]
fn capabilities_reports_revision_and_declared_keys() {
    let tmp = TempDir::new("caps");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let out = Command::new(ARL)
        .args(["capabilities", "--socket"])
        .arg(&sock)
        .output()
        .expect("ejecutar arl capabilities");
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["result"]["type"], "capabilities");
    assert_eq!(v["result"]["revision"], 0);
    assert_eq!(
        v["result"]["payload"]["schema_version"],
        "arl.capabilities/v1"
    );
    assert!(
        v["result"]["payload"]["config_keys"]
            .as_array()
            .unwrap()
            .len()
            >= 9
    );
}

#[test]
fn config_set_without_applicator_is_unsupported_without_mutation() {
    let tmp = TempDir::new("set");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    // Sin aplicador de hardware el set no se aplica: error honesto, sin ACK.
    let refused = raw_request(&sock, &set_line("set-1", 0, "3.5"));
    assert_eq!(refused["request_id"], "set-1");
    assert_eq!(refused["ok"], false, "{refused}");
    assert_eq!(refused["error"]["code"], "unsupported", "{refused}");

    let got = raw_request(&sock, &get_line("get-1", Some(KEY)));
    assert_eq!(got["ok"], true, "{got}");
    assert_eq!(got["result"]["revision"], 0);
    assert_eq!(got["result"]["payload"]["effective"]["value"], 0.0);

    let out = Command::new(ARL)
        .args(["config", "get", KEY, "--socket"])
        .arg(&sock)
        .output()
        .expect("ejecutar arl config get");
    assert_eq!(out.status.code(), Some(0));
}
#[test]
fn config_set_cli_refusal_exits_one_without_mutation() {
    let tmp = TempDir::new("refuse");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let out = Command::new(ARL)
        .args(["config", "set", KEY, "-6", "--rev", "0", "--socket"])
        .arg(&sock)
        .output()
        .expect("ejecutar arl config set");
    assert_eq!(
        out.status.code(),
        Some(1),
        "rechazo visible en el exit code"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "unsupported", "{v}");

    let caps = raw_request(&sock, r#"{"request_id":"cap-r","op":"capabilities"}"#);
    assert_eq!(caps["result"]["revision"], 0, "{caps}");
}
#[test]
fn repeated_request_id_replays_refusal_without_applying() {
    let tmp = TempDir::new("idem");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let first = raw_request(&sock, &set_line("idem-1", 0, "3.5"));
    let again = raw_request(&sock, &set_line("idem-1", 0, "3.5"));
    assert_eq!(first, again);
    assert_eq!(first["error"]["code"], "unsupported", "{first}");
    let caps = raw_request(&sock, r#"{"request_id":"cap-i","op":"capabilities"}"#);
    assert_eq!(caps["result"]["revision"], 0, "{caps}");
}
#[test]
fn unknown_key_and_invalid_value_return_correlated_errors() {
    let tmp = TempDir::new("badkey");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let unknown = raw_request(&sock, &get_line("nk-1", Some("no.existe")));
    assert_eq!(unknown["request_id"], "nk-1");
    assert_eq!(unknown["ok"], false);
    assert_eq!(unknown["error"]["code"], "not_found");

    let unknown_key = raw_request(
        &sock,
        r#"{"request_id":"nk-2","op":"config.get","args":{"key":"audio.nope"}}"#,
    );
    assert_eq!(unknown_key["request_id"], "nk-2");
    assert_eq!(unknown_key["error"]["code"], "not_found");

    // 0.3 no respeta el paso de 0.5 de audio.gain_db.
    let bad = raw_request(&sock, &set_line("bv-1", 0, "0.3"));
    assert_eq!(bad["request_id"], "bv-1");
    assert_eq!(bad["ok"], false);
    assert_eq!(bad["error"]["code"], "invalid");

    let out = Command::new(ARL)
        .args(["config", "set", "no.existe", "1", "--rev", "0", "--socket"])
        .arg(&sock)
        .output()
        .expect("ejecutar arl config set");
    assert_eq!(out.status.code(), Some(1));

    // El daemon sigue vivo y la revisión no cambió.
    let caps = raw_request(&sock, r#"{"request_id":"cap-1","op":"capabilities"}"#);
    assert_eq!(caps["result"]["revision"], 0);
}

#[test]
fn config_usage_errors_exit_two() {
    let out = Command::new(ARL)
        .args(["config", "set", KEY, "1", "--socket", "/tmp/x.sock"])
        .output()
        .expect("ejecutar arl config set sin --rev");
    assert_eq!(out.status.code(), Some(2));

    let out = Command::new(ARL)
        .args(["config", "frob"])
        .output()
        .expect("ejecutar arl config frob");
    assert_eq!(out.status.code(), Some(2));
}

/// Un cliente lento que envía fragmentos con pausas mayores que el tramo de
/// lectura y nunca termina la línea debe recibir `too_large` en vez de hacer
/// crecer la línea sin límite.
#[test]
fn slow_unterminated_line_is_rejected_as_too_large() {
    let tmp = TempDir::new("slowline");
    let sock = tmp.0.join("arl").join("control.sock");
    let _d = start_daemon(&sock);

    let mut writer = UnixStream::connect(&sock).expect("conectar");
    writer
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let reader_stream = writer.try_clone().expect("clonar socket");
    // Lector concurrente: la respuesta llega mientras seguimos escribiendo.
    let reader = std::thread::spawn(move || {
        let mut s = reader_stream;
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut resp = String::new();
        BufReader::new(&mut s)
            .read_line(&mut resp)
            .expect("leer respuesta");
        resp
    });

    // Hasta 1 MiB sin salto de línea; el tope de 64 KiB debe cortar antes.
    let chunk = vec![b'a'; 8 * 1024];
    for _ in 0..128 {
        if writer.write_all(&chunk).is_err() {
            break;
        }
        sleep(Duration::from_millis(30));
    }

    let resp = reader.join().expect("hilo lector");
    let v: serde_json::Value = serde_json::from_str(resp.trim_end()).expect("respuesta JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "too_large");
}
