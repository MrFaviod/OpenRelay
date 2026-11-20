//! El proceso `openrelay daemon` con archivos aportados comparte una revisión entre
//! el socket Unix del CLI y un cliente QUIC real.
//!
//! La CA, las hojas y la clave PKCS#8 nacen en este test y se borran con el
//! directorio temporal. No hay CA de producción ni material versionado.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::UdpSocket;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use openrelay_proto::control::{ConfigKey, ConfigValue, ErrorCode, ErrorMsg};
use openrelay_proto::wire::{ConfigSetMsg, ControlMessage};
use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{SessionClient, SessionLimits};
use rcgen::{
    date_time_ymd, BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const OPENRELAY: &str = env!("CARGO_BIN_EXE_openrelay");

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("openrelay-proc-{}\"", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("tmpdir");
        TempDir(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Leaf {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    pin: SpkiPin,
}

fn leaf(ca_cert: &rcgen::Certificate, ca_key: &KeyPair, name: &str, server: bool) -> Leaf {
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).expect("params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![if server {
        ExtendedKeyUsagePurpose::ServerAuth
    } else {
        ExtendedKeyUsagePurpose::ClientAuth
    }];
    params.not_before = date_time_ymd(2024, 1, 1);
    params.not_after = date_time_ymd(2035, 1, 1);
    let key = KeyPair::generate().expect("clave");
    let cert = params
        .signed_by(&key, ca_cert, ca_key)
        .expect("hoja firmada");
    let pin = SpkiPin::from_certificate_der(cert.der()).expect("pin");
    Leaf {
        cert_der: cert.der().as_ref().to_vec(),
        key_der: key.serialize_der(),
        pin,
    }
}

fn write_material(dir: &TempDir) -> (SpkiPin, SpkiPin) {
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("params CA");
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "openrelay-process-test-ca");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    ca_params.not_before = date_time_ymd(2024, 1, 1);
    ca_params.not_after = date_time_ymd(2035, 1, 1);
    let ca_key = KeyPair::generate().expect("clave CA");
    let ca_cert = ca_params.self_signed(&ca_key).expect("CA");

    let server = leaf(&ca_cert, &ca_key, "openrelay-process-server", true);
    let client = leaf(&ca_cert, &ca_key, "openrelay-process-client", false);

    fs::write(dir.path("server.der"), &server.cert_der).unwrap();
    fs::write(dir.path("server.key"), &server.key_der).unwrap();
    fs::write(dir.path("client.der"), &client.cert_der).unwrap();
    fs::write(dir.path("client.key"), &client.key_der).unwrap();
    fs::write(dir.path("ca.der"), ca_cert.der().as_ref()).unwrap();
    fs::write(dir.path("ca.key"), ca_key.serialize_der()).unwrap();
    fs::write(
        dir.path("identities.json"),
        format!(
            "{{\"version\":1,\"identities\":[{{\"spki\":\"{}\",\"mic\":false,\"control_read\":true,\"control_write\":true,\"return_audio\":false}}]}}\n",
            client.pin.to_hex()
        ),
    )
    .unwrap();
    fs::write(
        dir.path("grants.txt"),
        format!("{}\topenrelay-android-test\twrite\n", client.pin.to_hex()),
    )
    .unwrap();
    (server.pin, client.pin)
}

fn start(dir: &TempDir, sock: &Path) -> (Daemon, String) {
    let child = Command::new(OPENRELAY)
        .args(["daemon", "--socket"])
        .arg(sock)
        .args(["--quic", "127.0.0.1:0"])
        .args(["--identity-der"])
        .arg(dir.path("server.der"))
        .args(["--identity-key"])
        .arg(dir.path("server.key"))
        .args(["--client-ca"])
        .arg(dir.path("ca.der"))
        .args(["--client-ca-key"])
        .arg(dir.path("ca.key"))
        .args(["--identities"])
        .arg(dir.path("identities.json"))
        .args(["--control-grants"])
        .arg(dir.path("grants.txt"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("arrancar openrelay");
    let mut daemon = Daemon(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(sock).is_err() {
        assert!(Instant::now() < deadline, "el socket no apareció");
        sleep(Duration::from_millis(20));
    }
    let stderr = daemon.0.stderr.take().expect("stderr");
    let mut line = String::new();
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pin = None;
    while Instant::now() < deadline {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            sleep(Duration::from_millis(20));
            continue;
        }
        if let Some(found) = line.split("pin ").nth(1) {
            pin = Some(found.trim().to_string());
            break;
        }
    }
    (daemon, pin.expect("el proceso no imprimió el pin"))
}

fn cli(sock: &Path, args: &[&str]) -> serde_json::Value {
    let output = Command::new(OPENRELAY)
        .args(args)
        .args(["--socket"])
        .arg(sock)
        .output()
        .expect("openrelay");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "salida no JSON ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn drive(client: &mut SessionClient, socket: &UdpSocket, server: std::net::SocketAddr) {
    let now = Instant::now();
    let mut buf = [0u8; 2048];
    for _ in 0..64 {
        while let Some((_to, bytes)) = client.poll_transmit(now) {
            socket.send_to(&bytes, server).expect("enviar");
        }
        socket
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => client
                .handle_datagram(now, from, &buf[..len])
                .expect("datagrama"),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("recv: {error}"),
        }
    }
}

#[test]
fn process_shares_revision_between_cli_and_quic() {
    let dir = TempDir::new();
    let sock = dir.path("control.sock");
    let (server_pin, _client_pin) = write_material(&dir);
    let (daemon, printed) = start(&dir, &sock);
    assert_eq!(printed, server_pin.to_hex());

    // `audio.mute` has an applicator: the CLI write is applied and moves the
    // shared revision.
    let first = cli(
        &sock,
        &["config", "set", "audio.mute", "true", "--rev", "0"],
    );
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["result"]["revision"], 1, "{first}");

    let client_cert = fs::read(dir.path("client.der")).unwrap();
    let client_key = fs::read(dir.path("client.key")).unwrap();
    let ca_der = fs::read(dir.path("ca.der")).unwrap();
    let signing = rustls::crypto::ring::sign::any_ecdsa_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(client_key),
    ))
    .expect("clave cliente");
    let identity = Arc::new(
        identity::certified_key(
            vec![
                CertificateDer::from(client_cert),
                CertificateDer::from(ca_der.clone()),
            ],
            signing,
        )
        .expect("identidad cliente"),
    );
    let roots = identity::root_store([CertificateDer::from(ca_der)]).unwrap();
    let status = cli(&sock, &["status"]);
    let addr: std::net::SocketAddr = status["result"]["quic"]
        .as_str()
        .expect("addr")
        .parse()
        .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut client = SessionClient::connect(
        identity,
        roots,
        server_pin,
        SessionLimits::default(),
        Instant::now(),
        addr,
        "localhost",
    )
    .expect("cliente");

    let set = ControlMessage::ConfigSet(ConfigSetMsg {
        request_id: "quic-1".to_string(),
        expected_revision: 1,
        key: ConfigKey::new("audio.jitter_target_ms").expect("clave"),
        value: ConfigValue::Integer(10),
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reply = None;
    while Instant::now() < deadline && reply.is_none() {
        let _ = client.send_control(&set);
        drive(&mut client, &socket, addr);
        reply = client.recv_control();
    }
    match reply.expect("sin respuesta QUIC") {
        ControlMessage::Error(ErrorMsg { code, .. }) => {
            assert_eq!(code, ErrorCode::Unsupported, "rechazo honesto por QUIC");
        }
        other => panic!("se esperaba error unsupported, llegó {other:?}"),
    }

    let caps = cli(&sock, &["capabilities"]);
    assert_eq!(caps["result"]["revision"], 1, "{caps}");
    drop(daemon);
}
