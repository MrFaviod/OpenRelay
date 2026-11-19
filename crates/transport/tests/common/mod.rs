//! Fixtures test-only: CA local, hojas ECDSA P-256, signer que cuenta firmas y
//! un enlace en memoria que transporta datagramas QUIC reales entre dos peers
//! sans-IO. No hay PEM ni claves de producción en el repo.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use openrelay_transport::enrollment::{EnrollClient, EnrollError, EnrollServer, EnrollmentToken};
use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{SessionClient, SessionServer, TransportError};
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, RemoteKeyPair, PKCS_ECDSA_P256_SHA256,
};
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::{CertificateDer, SubjectPublicKeyInfoDer};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};

pub const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44331);
pub const CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44332);

/// Firma ECDSA P-256/SHA-256 con contador. El material es de test y vive en
/// memoria; no es un Keystore ni exporta nada de producción.
#[derive(Debug)]
pub struct CountingSigner {
    key: Arc<EcdsaKeyPair>,
    spki: Vec<u8>,
    signatures: Arc<AtomicUsize>,
}

impl CountingSigner {
    pub fn new(key_pair: &KeyPair) -> Self {
        let rng = ring::rand::SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            &key_pair.serialize_der(),
            &rng,
        )
        .expect("clave ECDSA P-256 válida");
        Self {
            key: Arc::new(key),
            spki: key_pair.public_key_der(),
            signatures: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn signatures(&self) -> usize {
        self.signatures.load(Ordering::SeqCst)
    }
}

impl SigningKey for CountingSigner {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if !offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256) {
            return None;
        }
        Some(Box::new(CountedSigner {
            key: Arc::clone(&self.key),
            signatures: Arc::clone(&self.signatures),
        }))
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.spki.clone()))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

#[derive(Debug)]
struct CountedSigner {
    key: Arc<EcdsaKeyPair>,
    signatures: Arc<AtomicUsize>,
}

impl Signer for CountedSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let rng = ring::rand::SystemRandom::new();
        let signature = self
            .key
            .sign(&rng, message)
            .map_err(|_| rustls::Error::General("firma de test falló".into()))?;
        self.signatures.fetch_add(1, Ordering::SeqCst);
        Ok(signature.as_ref().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

pub struct TestCa {
    pub cert: Certificate,
    key: KeyPair,
}

impl TestCa {
    pub fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params CA");
        params
            .distinguished_name
            .push(DnType::CommonName, "openrelay-test-ca");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before = date_time_ymd(1975, 1, 1);
        params.not_after = date_time_ymd(4096, 1, 1);
        let key = KeyPair::generate().expect("clave CA");
        let cert = params.self_signed(&key).expect("CA autofirmada");
        Self { cert, key }
    }

    pub fn leaf(&self, common_name: &str, server: bool, expired: bool) -> TestLeaf {
        let mut params =
            CertificateParams::new(vec!["localhost".to_string()]).expect("params hoja");
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![if server {
            ExtendedKeyUsagePurpose::ServerAuth
        } else {
            ExtendedKeyUsagePurpose::ClientAuth
        }];
        if expired {
            params.not_before = date_time_ymd(1975, 1, 1);
            params.not_after = date_time_ymd(2020, 1, 1);
        }
        let key = KeyPair::generate().expect("clave hoja");
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .expect("hoja firmada por la CA");
        TestLeaf::new(cert, key)
    }

    pub fn cert_der(&self) -> CertificateDer<'static> {
        self.cert.der().clone()
    }

    /// Firma una hoja de cliente para una clave pública P-256 ya existente.
    ///
    /// Es el mismo patrón que usa el daemon para (`ENROLL_REQUEST`)`: la CA
    /// firma el TBS y la clave privada del dispositivo nunca sale de él.
    pub fn issue_client_leaf(&self, spki_der: &[u8]) -> Vec<u8> {
        let point = p256_point(spki_der);
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params hoja");
        params
            .distinguished_name
            .push(DnType::CommonName, "openrelay-device");
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before = date_time_ymd(1975, 1, 1);
        params.not_after = date_time_ymd(4096, 1, 1);
        let key = KeyPair::from_remote(Box::new(PublicOnlyKey { point })).expect("clave remota");
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .expect("hoja de cliente firmada");
        cert.der().to_vec()
    }
}

/// Punto P-256 sin comprimir a partir del SPKI DER (prefijo fijo de 26 bytes).
fn p256_point(spki: &[u8]) -> Vec<u8> {
    const PREFIX: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    assert_eq!(spki.len(), 91, "SPKI P-256 sin comprimir");
    assert_eq!(spki[..26], PREFIX, "prefijo SPKI P-256");
    assert_eq!(spki[26], 0x04, "punto sin comprimir");
    spki[26..].to_vec()
}

/// Sólo la parte pública: rcgen firma el certificado con la CA, no con esta
/// clave, así que `sign` no se usa.
struct PublicOnlyKey {
    point: Vec<u8>,
}

impl RemoteKeyPair for PublicOnlyKey {
    fn public_key(&self) -> &[u8] {
        &self.point
    }

    fn sign(&self, _msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        Err(rcgen::Error::RingUnspecified)
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

pub struct TestLeaf {
    pub cert: Certificate,
    pub signer: Arc<CountingSigner>,
    pub pin: SpkiPin,
}

impl TestLeaf {
    fn new(cert: Certificate, key: KeyPair) -> Self {
        let pin = SpkiPin::from_certificate_der(cert.der()).expect("SPKI de la hoja");
        Self {
            cert,
            signer: Arc::new(CountingSigner::new(&key)),
            pin,
        }
    }

    pub fn certified(&self, ca: &TestCa) -> Arc<CertifiedKey> {
        Arc::new(
            identity::certified_key(
                vec![self.cert.der().clone(), ca.cert_der()],
                self.signer.clone(),
            )
            .expect("CertifiedKey de test"),
        )
    }

    pub fn spki_sha256(&self) -> SpkiPin {
        self.pin
    }
}

pub fn roots(ca: &TestCa) -> Arc<rustls::RootCertStore> {
    identity::root_store([ca.cert_der()]).expect("root store de test")
}

/// SPKI DER de una clave de cliente de prueba (la que iría en `enroll.request`).
pub fn client_spki_der() -> Vec<u8> {
    KeyPair::generate().expect("clave cliente").public_key_der()
}

pub fn token() -> EnrollmentToken {
    EnrollmentToken::generate().expect("token del sistema")
}

/// Peer sans-IO que el driver en memoria puede bombear.
pub trait WirePeer {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)>;
    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String>;
}

impl WirePeer for SessionClient {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        SessionClient::poll_transmit(self, now)
    }

    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String> {
        SessionClient::handle_datagram(self, now, from, payload).map_err(|error| error.to_string())
    }
}

impl WirePeer for SessionServer {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        SessionServer::poll_transmit(self, now)
    }

    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String> {
        SessionServer::handle_datagram(self, now, from, payload).map_err(|error| error.to_string())
    }
}

impl WirePeer for EnrollClient {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        EnrollClient::poll_transmit(self, now)
    }

    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String> {
        EnrollClient::handle_datagram(self, now, from, payload).map_err(|error| error.to_string())
    }
}

impl WirePeer for EnrollServer {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        EnrollServer::poll_transmit(self, now)
    }

    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String> {
        EnrollServer::handle_datagram(self, now, from, payload).map_err(|error| error.to_string())
    }
}

/// Bombea un solo vuelo completo `from → to` (hasta 256 transmisiones).
pub fn drain<A: WirePeer, B: WirePeer>(
    from: &mut A,
    from_addr: SocketAddr,
    to: &mut B,
    now: Instant,
) -> Result<(), String> {
    for _ in 0..256 {
        let Some((_destination, payload)) = from.poll_transmit(now) else {
            return Ok(());
        };
        to.handle_datagram(now, from_addr, &payload)?;
    }
    Err("bombeo de datagramas no termina".to_string())
}

/// Intercambia vuelos hasta que ninguno de los dos tenga algo que enviar.
pub fn exchange<A: WirePeer, B: WirePeer>(
    client: &mut A,
    client_addr: SocketAddr,
    server: &mut B,
    server_addr: SocketAddr,
    rounds: usize,
) -> Result<(), String> {
    let now = Instant::now();
    for _ in 0..rounds {
        drain(client, client_addr, server, now)?;
        drain(server, server_addr, client, now)?;
    }
    Ok(())
}

pub fn session_setup(
    client_leaf: &TestLeaf,
    server_leaf: &TestLeaf,
    ca: &TestCa,
    acl_client_leaf: Option<&TestLeaf>,
) -> Result<(SessionClient, SessionServer), String> {
    let server_identity = server_leaf.certified(ca);
    let mut acl = openrelay_transport::quinn::Acl::new();
    if let Some(leaf) = acl_client_leaf {
        acl.allow(
            leaf.spki_sha256(),
            openrelay_transport::quinn::Capabilities::none()
                .with_mic()
                .with_control_read()
                .with_control_write(),
        );
    }
    let server = SessionServer::new(
        server_identity,
        roots(ca),
        acl,
        openrelay_transport::quinn::SessionLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    let client = SessionClient::connect(
        client_leaf.certified(ca),
        roots(ca),
        server_leaf.pin,
        openrelay_transport::quinn::SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .map_err(|error| error.to_string())?;
    Ok((client, server))
}

pub fn enrollment_setup(
    ca: &TestCa,
    token: EnrollmentToken,
) -> Result<(EnrollClient, EnrollServer), String> {
    let leaf = ca.leaf("openrelay-enroll-test", true, false);
    let mut server = EnrollServer::new(leaf.certified(ca)).map_err(|error| error.to_string())?;
    server
        .open_window(
            token.clone(),
            std::time::Duration::from_secs(120),
            Instant::now(),
        )
        .map_err(|error| error.to_string())?;
    let client = EnrollClient::connect(
        roots(ca),
        leaf.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .map_err(|error| error.to_string())?;
    Ok((client, server))
}

pub fn error_is_closed(error: &EnrollError) -> bool {
    matches!(error, EnrollError::Closed)
}

pub fn session_error_is_closed(error: &TransportError) -> bool {
    matches!(error, TransportError::Closed)
}
