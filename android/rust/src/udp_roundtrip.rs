//! Roundtrip de host contra un servidor fixture en un hilo aparte.
//!
//! No es el daemon de producto. La CA existe sólo aquí, nunca en el cliente.
//! El cliente es el de producto (`client::ProductClient`) y firma con una
//! clave local que imita el `SigningKey` del Keystore, sin exportar la privada.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{BridgeEvent, ProductClient, Provision};
use arl_proto::control::{AuthorizedPeer, ConfigState, ControlSession, PeerRole, RemoteAction};
use arl_proto::wire::ControlMessage;
use arl_transport::identity::{self, SpkiPin};
use arl_transport::quinn::{
    Acl, Capabilities, ServerEvent, SessionId, SessionLimits, SessionServer,
};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::{CertificateDer, SubjectPublicKeyInfoDer};
use rustls::sign::{Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};

#[test]
fn roundtrip_against_external_fixture() {
    let (endpoint, material, key, _keep, _observed) = spawn_fixture();
    let parts: Vec<&str> = material.trim().split(' ').collect();
    assert_eq!(parts.len(), 4, "CA pin cadena-cliente cadena-servidor");
    let ca = decode_der(parts[0]);
    let pin = SpkiPin::from_hex(parts[1]).expect("pin");
    let client_chain = vec![decode_der(parts[2]), ca.clone()];

    let mut client = ProductClient::open(Provision {
        endpoint,
        server_name: "localhost".into(),
        ca_der: ca,
        pin,
        chain: client_chain,
        key,
    })
    .expect("open");
    client.wait_authorized().expect("autorizado");

    client.hello().expect("hello");
    let snapshot_id = client.config_get(None).expect("snapshot");
    let snapshot = client.wait_control(&snapshot_id).expect("ack snapshot");
    match snapshot {
        BridgeEvent::Ack {
            applied,
            effective_json,
            ..
        } => {
            assert!(applied, "snapshot no aplicado");
            assert_eq!(effective_json, "{\"snapshot\":true}");
        }
        other => panic!("snapshot no es ack: {other:?}"),
    }

    let set_id = client.config_set_gain(0, 3.0).expect("set");
    let set = client.wait_control(&set_id).expect("ack set");
    match set {
        BridgeEvent::Ack {
            requested_json,
            effective_json,
            applied,
            ..
        } => {
            assert!(applied, "set no aplicado");
            assert_ne!(
                requested_json, effective_json,
                "requested y effective no pueden colapsar"
            );
            assert!(
                requested_json.contains("3"),
                "requested perdió el pedido: {requested_json}"
            );
            assert!(
                effective_json.contains("2.5"),
                "effective no es el tope del fixture: {effective_json}"
            );
        }
        other => panic!("set no es ack: {other:?}"),
    }

    client.close().expect("cierre");
    client.close().expect("cierre idempotente");
}

/// Lo que el servidor fixture observó del cliente, en orden de llegada.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Observation {
    /// Features declaradas en un `capabilities` del cliente.
    Capabilities(Vec<String>),
    /// `type` del datagrama de audio ya parseado.
    Audio(u8),
}

type Observed = Arc<Mutex<Vec<Observation>>>;

/// El puente anuncia Opus en `capabilities` antes de que el servidor reciba
/// ningún frame `type=2`. Sin ese anuncio el daemon descarta todo el audio.
#[test]
fn client_advertises_opus_before_sending_it() {
    let (endpoint, material, key, _keep, observed) = spawn_fixture();
    let parts: Vec<&str> = material.trim().split(' ').collect();
    let ca = decode_der(parts[0]);
    let pin = SpkiPin::from_hex(parts[1]).expect("pin");
    let client_chain = vec![decode_der(parts[2]), ca.clone()];

    let mut client = ProductClient::open(Provision {
        endpoint,
        server_name: "localhost".into(),
        ca_der: ca,
        pin,
        chain: client_chain,
        key,
    })
    .expect("open");
    client.wait_authorized().expect("autorizado");

    // El perfil se fija antes del hello: la primera capabilities ya lo declara.
    client.set_codec(1).expect("codec opus");
    client.hello().expect("hello");

    let samples = [0i16; 240];
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline && !saw_opus_audio(&observed) {
        let _ = client.send_audio(&samples);
        thread::sleep(Duration::from_millis(10));
    }
    client.close().expect("cierre");

    let observed = observed.lock().expect("observaciones").clone();
    let caps_at = observed
        .iter()
        .position(|item| {
            matches!(item, Observation::Capabilities(features)
                if features.iter().any(|name| name == "audio.opus-48k-mono"))
        })
        .unwrap_or_else(|| panic!("capabilities con opus ausentes: {observed:?}"));
    let audio_at = observed
        .iter()
        .position(|item| matches!(item, Observation::Audio(2)))
        .unwrap_or_else(|| panic!("sin frame opus en el servidor: {observed:?}"));
    assert!(
        caps_at < audio_at,
        "el frame opus precedió al anuncio: {observed:?}"
    );
}

fn saw_opus_audio(observed: &Observed) -> bool {
    observed
        .lock()
        .expect("observaciones")
        .iter()
        .any(|item| matches!(item, Observation::Audio(2)))
}

fn read_line(stream: &mut impl Read, into: &mut String) {
    into.clear();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).expect("lee fixture");
        if n == 0 || byte[0] == b'\n' {
            break;
        }
        into.push(byte[0] as char);
    }
}

fn decode_der(hex_text: &str) -> CertificateDer<'static> {
    CertificateDer::from(unhex(hex_text).expect("hex"))
}

fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("hex impar".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn signing_key(leaf: &Leaf) -> Arc<dyn SigningKey> {
    let der = leaf.key.serialize_der();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &der, &SystemRandom::new())
        .expect("pkcs8");
    Arc::new(LocalKey {
        key: Arc::new(key),
        spki: leaf.key.public_key_der(),
    })
}

#[derive(Debug)]
struct LocalKey {
    key: Arc<EcdsaKeyPair>,
    spki: Vec<u8>,
}

impl SigningKey for LocalKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if !offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256) {
            return None;
        }
        Some(Box::new(LocalSigner {
            key: Arc::clone(&self.key),
        }))
    }
    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.spki.as_slice()))
    }
    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

#[derive(Debug)]
struct LocalSigner {
    key: Arc<EcdsaKeyPair>,
}

impl Signer for LocalSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.key
            .sign(&SystemRandom::new(), message)
            .map(|signature| signature.as_ref().to_vec())
            .map_err(|_| rustls::Error::General("firma fixture".into()))
    }
    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

/// Servidor fixture en un hilo aparte. Anuncia endpoint y material por TCP
/// y se detiene cuando el padre cierra esa conexión.
fn spawn_fixture() -> (
    SocketAddr,
    String,
    Arc<dyn SigningKey>,
    std::net::TcpStream,
    Observed,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("tcp fixture");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = std::sync::mpsc::channel();
    let observed: Observed = Arc::new(Mutex::new(Vec::new()));
    let fixture_observed = Arc::clone(&observed);
    thread::spawn(move || run_fixture(port, tx, fixture_observed));
    let (mut stream, _) = listener.accept().expect("fixture acepta");
    let mut line = String::new();
    read_line(&mut stream, &mut line);
    let endpoint: SocketAddr = line.trim().parse().expect("endpoint del fixture");
    let mut material = String::new();
    read_line(&mut stream, &mut material);
    let key = rx.recv().expect("clave del cliente");
    (endpoint, material, key, stream, observed)
}

fn run_fixture(
    port: u16,
    key_out: std::sync::mpsc::Sender<Arc<dyn SigningKey>>,
    observed: Observed,
) {
    let mut tcp = std::net::TcpStream::connect(("127.0.0.1", port)).expect("tcp padre");
    let ca = test_ca();
    let server_leaf = leaf(&ca, "arl-fixture", true);
    let client_leaf = leaf(&ca, "arl-client", false);
    let server_identity = certified(&server_leaf, &ca);
    key_out
        .send(signing_key(&client_leaf))
        .expect("entrega clave");
    let pin = SpkiPin::from_certificate_der(client_leaf.cert.der()).expect("pin cliente");
    let mut acl = Acl::new();
    acl.allow(
        pin,
        Capabilities::none()
            .with_mic()
            .with_control_read()
            .with_control_write(),
    );
    let mut server = SessionServer::new(
        server_identity,
        identity::root_store([CertificateDer::from(ca.cert.der().to_vec())]).expect("roots"),
        acl,
        SessionLimits::default(),
    )
    .expect("server");
    let socket = UdpSocket::bind("127.0.0.1:0").expect("udp");
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("timeout");
    let endpoint = socket.local_addr().expect("local");
    writeln!(
        tcp,
        "{endpoint}\n{} {} {} {}",
        to_hex(ca.cert.der()),
        SpkiPin::from_certificate_der(server_leaf.cert.der())
            .expect("pin server")
            .to_hex(),
        to_hex(client_leaf.cert.der()),
        to_hex(server_leaf.cert.der()),
    )
    .expect("anuncia");

    let sessions: Mutex<Vec<(SessionId, ControlSession)>> = Mutex::new(Vec::new());
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 2048];
    let mut probe = [0u8; 1];
    while Instant::now() < deadline {
        let now = Instant::now();
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let _ = server.handle_datagram(now, from, &buf[..n]);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                tcp.set_nonblocking(true).ok();
                if let Ok(0) = tcp.read(&mut probe) {
                    break;
                }
            }
            Err(_) => break,
        }
        // Sin esto el servidor sans-IO nunca avanza sus temporizadores: el
        // handshake y los ACK sólo progresarían cuando llega otro datagrama, y
        // cada reintento del cliente duplica la espera hasta agotar el plazo.
        let _ = server.handle_timeout(Instant::now());
        while let Some((to, datagram)) = server.poll_transmit(Instant::now()) {
            let _ = socket.send_to(&datagram, to);
        }
        while let Some(event) = server.next_event() {
            if let ServerEvent::Authorized { session, .. } = event {
                let control = ControlSession::new(
                    AuthorizedPeer::new(
                        "android-fixture",
                        PeerRole::Android,
                        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
                    ),
                    ConfigState::new(),
                    Box::new(ClampGain),
                );
                sessions.lock().expect("sesiones").push((session, control));
            }
        }
        let mut guard = sessions.lock().expect("sesiones");
        for (session, control) in guard.iter_mut() {
            while let Some(message) = server.recv_control(*session) {
                if let ControlMessage::Capabilities(caps) = &message {
                    observed
                        .lock()
                        .expect("observaciones")
                        .push(Observation::Capabilities(
                            caps.capabilities
                                .features
                                .iter()
                                .map(|feature| feature.as_str().to_string())
                                .collect(),
                        ));
                }
                let outcome = control.handle(&message, 0);
                if let Some(reply) = outcome.response {
                    let _ = server.send_control(*session, &reply);
                }
            }
            while let Some(payload) = server.recv_audio_payload(*session) {
                observed
                    .lock()
                    .expect("observaciones")
                    .push(Observation::Audio(payload.frame_type()));
            }
        }
        drop(guard);
    }
}

struct Leaf {
    cert: rcgen::Certificate,
    key: KeyPair,
}

struct Ca {
    cert: rcgen::Certificate,
    key: KeyPair,
}

fn test_ca() -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params
        .distinguished_name
        .push(DnType::CommonName, "arl-fixture-ca");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().expect("ca key");
    let cert = params.self_signed(&key).expect("ca");
    Ca { cert, key }
}

fn leaf(ca: &Ca, name: &str, server: bool) -> Leaf {
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).expect("params");
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![if server {
        ExtendedKeyUsagePurpose::ServerAuth
    } else {
        ExtendedKeyUsagePurpose::ClientAuth
    }];
    let key = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, &ca.cert, &ca.key).expect("leaf");
    Leaf { cert, key }
}

fn certified(leaf: &Leaf, ca: &Ca) -> Arc<rustls::sign::CertifiedKey> {
    Arc::new(
        identity::certified_key(
            vec![
                CertificateDer::from(leaf.cert.der().to_vec()),
                CertificateDer::from(ca.cert.der().to_vec()),
            ],
            signing_key(leaf),
        )
        .expect("certified"),
    )
}

/// Política de fixture: `audio.gain_db` pedido por encima de 2.5 se fija en 2.5.
struct ClampGain;

impl arl_proto::control::EffectivePolicy for ClampGain {
    fn effective(
        &self,
        key: &arl_proto::control::ConfigKey,
        requested: &arl_proto::control::ConfigValue,
    ) -> Result<arl_proto::control::ConfigValue, arl_proto::control::ErrorCode> {
        if key.as_str() == "audio.gain_db" {
            if let arl_proto::control::ConfigValue::Number(value) = requested {
                if *value > 2.5 {
                    return Ok(arl_proto::control::ConfigValue::Number(2.5));
                }
            }
        }
        Ok(requested.clone())
    }
}
