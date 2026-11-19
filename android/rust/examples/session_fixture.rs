//! Fixture de host SÓLO TEST. No es producto y no provisiona al cliente.
//!
//! Escucha QUIC de sesión en UDP `0.0.0.0:<puerto dinámico>` y emite una hoja
//! `clientAuth` para el SPKI público que el test le entrega por archivo. La
//! privada del dispositivo nunca llega: la CA de test firma el certificado y
//! se tira al salir. El canal de preparación es un archivo de texto con vida
//! acotada, fuera de `arl.control/v1`.
//!
//! Uso: `session_fixture <directorio>`
//! Escribe `ready` (puerto, CA DER, pin del servidor, cadena del servidor) y
//! espera `spki` (SPKI DER en hex). Responde `leaf` (hoja DER en hex) y atiende
//! el control hasta que aparece `stop` o pasan 150 s.

use std::fs;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arl_proto::control::{AuthorizedPeer, ConfigState, ControlSession, PeerRole, RemoteAction};
use arl_transport::identity::{self, SpkiPin};
use arl_transport::quinn::{
    Acl, Capabilities, ServerEvent, SessionId, SessionLimits, SessionServer,
};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, RemoteKeyPair, PKCS_ECDSA_P256_SHA256,
};
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::{CertificateDer, SubjectPublicKeyInfoDer};
use rustls::sign::{Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};

/// Prefijo SPKI de P-256 sin comprimir, verificado contra rcgen 0.13.1.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
const SPKI_LEN: usize = 91;
const POINT_OFFSET: usize = 26;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--self-check") => self_check(),
        Some(dir) => {
            if let Err(error) = run(std::path::Path::new(dir)) {
                let _ = fs::write(std::path::Path::new(dir).join("fail"), &error);
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        None => {
            eprintln!("uso: session_fixture <directorio> | --self-check");
            std::process::exit(2);
        }
    }
}

/// Emite una hoja para un SPKI generado aquí y comprueba que el certificado
/// publicado lleva exactamente ese SPKI. La privada no se escribe.
fn self_check() {
    let ca = test_ca().expect("ca");
    let device = KeyPair::generate().expect("device");
    let spki = device.public_key_der();
    let cert = issue_client_leaf(&ca, &spki).expect("leaf");
    let issued = identity::extract_spki_der(cert.der()).expect("spki del certificado");
    assert_eq!(issued, spki.as_slice(), "la hoja no lleva el SPKI pedido");
    println!("OK");
}

fn run(dir: &std::path::Path) -> Result<(), String> {
    let ca = test_ca()?;
    let server_leaf = leaf(&ca, "arl-fixture", true)?;
    let server_identity = certified(&server_leaf, &ca)?;
    let server_pin = SpkiPin::from_certificate_der(server_leaf.cert.der()).map_err(show)?;

    let socket = UdpSocket::bind("0.0.0.0:0").map_err(show)?;
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .map_err(show)?;
    let port = socket.local_addr().map_err(show)?.port();

    fs::write(
        dir.join("ready"),
        format!(
            "{port} {} {} {}\n",
            to_hex(ca.cert.der()),
            server_pin.to_hex(),
            to_hex(server_leaf.cert.der()),
        ),
    )
    .map_err(show)?;

    let spki = read_spki(dir)?;
    let client_cert = issue_client_leaf(&ca, &spki)?;
    fs::write(dir.join("leaf"), format!("{}\n", to_hex(client_cert.der()))).map_err(show)?;

    let pin = SpkiPin::from_certificate_der(client_cert.der()).map_err(show)?;
    let mut acl = Acl::new();
    acl.allow(
        pin,
        Capabilities::none()
            .with_control_read()
            .with_control_write(),
    );
    let mut server = SessionServer::new(
        server_identity,
        identity::root_store([CertificateDer::from(ca.cert.der().to_vec())]).map_err(show)?,
        acl,
        SessionLimits::default(),
    )
    .map_err(show)?;

    let sessions: Mutex<Vec<(SessionId, ControlSession)>> = Mutex::new(Vec::new());
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline && !dir.join("stop").exists() {
        let now = Instant::now();
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let _ = server.handle_datagram(now, from, &buf[..n]);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        while let Some((to, datagram)) = server.poll_transmit(now) {
            let _ = socket.send_to(&datagram, to);
        }
        while let Some(event) = server.next_event() {
            if let ServerEvent::Authorized { session, .. } = event {
                let control = ControlSession::new(
                    AuthorizedPeer::new(
                        "fixture",
                        PeerRole::Android,
                        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
                    ),
                    ConfigState::new(),
                    Box::new(ClampGain),
                );
                sessions.lock().map_err(show)?.push((session, control));
            }
        }
        let mut guard = sessions.lock().map_err(show)?;
        for (session, control) in guard.iter_mut() {
            while let Some(message) = server.recv_control(*session) {
                let outcome = control.handle(&message, 0);
                if let Some(reply) = outcome.response {
                    let _ = server.send_control(*session, &reply);
                }
            }
        }
    }
    Ok(())
}

/// Espera el SPKI que escribe el test. 60 s y se rinde.
fn read_spki(dir: &std::path::Path) -> Result<Vec<u8>, String> {
    let path = dir.join("spki");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(&path) {
            if !text.trim().is_empty() {
                return unhex(text.trim());
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err("el test no entregó el SPKI".into())
}

/// Hoja `clientAuth` cuyo SPKI es el del dispositivo. La firma la pone la CA.
fn issue_client_leaf(ca: &Ca, spki: &[u8]) -> Result<rcgen::Certificate, String> {
    let point = p256_point(spki)?;
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(show)?;
    params
        .distinguished_name
        .push(DnType::CommonName, "arl-device");
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let key = KeyPair::from_remote(Box::new(PublicOnlyKey { point })).map_err(show)?;
    params.signed_by(&key, &ca.cert, &ca.key).map_err(show)
}

/// Rechaza cualquier SPKI que no sea P-256 sin comprimir de 91 bytes.
fn p256_point(spki: &[u8]) -> Result<Vec<u8>, String> {
    if spki.len() != SPKI_LEN || spki[..POINT_OFFSET] != P256_SPKI_PREFIX {
        return Err(format!(
            "SPKI no es P-256 sin comprimir ({} bytes)",
            spki.len()
        ));
    }
    if spki[POINT_OFFSET] != 0x04 {
        return Err("punto P-256 comprimido o inválido".into());
    }
    Ok(spki[POINT_OFFSET..].to_vec())
}

/// Clave pública sin privada. `sign` sólo se usa para el TBS del certificado
/// que emite la CA de test; no hay secreto que exportar.
struct PublicOnlyKey {
    point: Vec<u8>,
}

impl RemoteKeyPair for PublicOnlyKey {
    fn public_key(&self) -> &[u8] {
        &self.point
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        let signing =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new())
                .map_err(|_| rcgen::Error::RingUnspecified)?;
        let pair = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            signing.as_ref(),
            &SystemRandom::new(),
        )
        .map_err(|_| rcgen::Error::RingUnspecified)?;
        pair.sign(&SystemRandom::new(), message)
            .map(|signature| signature.as_ref().to_vec())
            .map_err(|_| rcgen::Error::RingUnspecified)
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

/// `audio.gain_db` por encima de 2.5 queda en 2.5. Es política de fixture,
/// no del hardware.
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

struct Ca {
    cert: rcgen::Certificate,
    key: KeyPair,
}

struct Leaf {
    cert: rcgen::Certificate,
    key: KeyPair,
}

fn test_ca() -> Result<Ca, String> {
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(show)?;
    params
        .distinguished_name
        .push(DnType::CommonName, "arl-test-ca");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let key = KeyPair::generate().map_err(show)?;
    let cert = params.self_signed(&key).map_err(show)?;
    Ok(Ca { cert, key })
}

fn leaf(ca: &Ca, name: &str, server: bool) -> Result<Leaf, String> {
    let mut params = CertificateParams::new(vec!["localhost".to_string()]).map_err(show)?;
    params.distinguished_name.push(DnType::CommonName, name);
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![if server {
        ExtendedKeyUsagePurpose::ServerAuth
    } else {
        ExtendedKeyUsagePurpose::ClientAuth
    }];
    let key = KeyPair::generate().map_err(show)?;
    let cert = params.signed_by(&key, &ca.cert, &ca.key).map_err(show)?;
    Ok(Leaf { cert, key })
}

fn certified(leaf: &Leaf, ca: &Ca) -> Result<Arc<rustls::sign::CertifiedKey>, String> {
    Ok(Arc::new(
        identity::certified_key(
            vec![
                CertificateDer::from(leaf.cert.der().to_vec()),
                CertificateDer::from(ca.cert.der().to_vec()),
            ],
            signing_key(leaf)?,
        )
        .map_err(show)?,
    ))
}

fn signing_key(leaf: &Leaf) -> Result<Arc<dyn SigningKey>, String> {
    let der = leaf.key.serialize_der();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &der, &SystemRandom::new())
        .map_err(show)?;
    Ok(Arc::new(LocalKey {
        key: Arc::new(key),
        spki: leaf.key.public_key_der(),
    }))
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

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
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

fn show<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}
