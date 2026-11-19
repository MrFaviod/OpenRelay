//! Regresiones E: topes de conexiones concurrentes por IP y por identidad.
//!
//! Módulo hijo de `quinn`: monta el `SessionServer` real, completa handshakes
//! mTLS+ALPN con clientes reales y elige la dirección de origen que ve el
//! servidor. Comprueba que el quinto peer de una misma IP recibe el
//! `CONNECTION_REFUSED` de quinn, que el rechazo por tope de IP no gasta un
//! intento de handshake, que la tercera sesión de una misma identidad se cierra
//! con la denegación del servidor y que una identidad distinta conserva su
//! cuota. Las fixtures (CA, hojas, ACL) son test-only y viven en memoria. No
//! mide caudal ni coste de CPU: no es una prueba de carga.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use rustls::RootCertStore;

use super::{
    Acl, Capabilities, ClientEvent, RejectReason, ServerEvent, SessionClient, SessionLimits,
    SessionServer, MAX_SESSIONS_PER_IDENTITY, MAX_SESSIONS_PER_IP,
};
use crate::identity::{self, SpkiPin};

const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44341);
/// Rondas de bombeo: sobran para un handshake mTLS completo en memoria.
const ROUNDS: usize = 32;

// ---------------------------------------------------------------------------
// Fixtures test-only: CA local y hojas ECDSA P-256, claves sólo en memoria.
// ---------------------------------------------------------------------------

struct TestCa {
    cert: Certificate,
    key: KeyPair,
}

impl TestCa {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params CA");
        params
            .distinguished_name
            .push(DnType::CommonName, "openrelay-test-ca-f9e");
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

    fn leaf(&self, common_name: &str, server: bool) -> TestLeaf {
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
        params.not_before = date_time_ymd(1975, 1, 1);
        params.not_after = date_time_ymd(4096, 1, 1);
        let key = KeyPair::generate().expect("clave hoja");
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .expect("hoja firmada por la CA");
        let pin = SpkiPin::from_certificate_der(cert.der()).expect("SPKI de la hoja");
        TestLeaf { cert, key, pin }
    }

    fn cert_der(&self) -> CertificateDer<'static> {
        self.cert.der().clone()
    }
}

struct TestLeaf {
    cert: Certificate,
    key: KeyPair,
    pin: SpkiPin,
}

impl TestLeaf {
    fn der(&self) -> CertificateDer<'static> {
        self.cert.der().clone()
    }

    fn certified(&self, ca: &TestCa) -> Arc<CertifiedKey> {
        let pkcs8 = PrivatePkcs8KeyDer::from(self.key.serialize_der());
        let signer = any_ecdsa_type(&PrivateKeyDer::Pkcs8(pkcs8)).expect("signer ECDSA de test");
        Arc::new(
            identity::certified_key(vec![self.der(), ca.cert_der()], signer)
                .expect("CertifiedKey de test"),
        )
    }
}

fn roots(ca: &TestCa) -> Arc<RootCertStore> {
    identity::root_store([ca.cert_der()]).expect("root store de test")
}

struct Fixture {
    ca: TestCa,
    server_leaf: TestLeaf,
    client_leaf: TestLeaf,
}

impl Fixture {
    fn new() -> Self {
        let ca = TestCa::new();
        let server_leaf = ca.leaf("openrelay-server-f9e", true);
        let client_leaf = ca.leaf("openrelay-client-f9e", false);
        Self {
            ca,
            server_leaf,
            client_leaf,
        }
    }

    fn leaf(&self, common_name: &str) -> TestLeaf {
        self.ca.leaf(common_name, false)
    }

    fn server(&self) -> SessionServer {
        self.server_allowing(&[self.client_leaf.pin])
    }

    fn server_allowing(&self, pins: &[SpkiPin]) -> SessionServer {
        let mut acl = Acl::new();
        for pin in pins {
            acl.allow(
                *pin,
                Capabilities::none()
                    .with_mic()
                    .with_control_read()
                    .with_control_write(),
            );
        }
        SessionServer::new(
            self.server_leaf.certified(&self.ca),
            roots(&self.ca),
            acl,
            SessionLimits::default(),
        )
        .expect("servidor de sesión")
    }

    fn client(&self) -> SessionClient {
        self.client_for(&self.client_leaf)
    }

    fn client_for(&self, leaf: &TestLeaf) -> SessionClient {
        SessionClient::connect(
            leaf.certified(&self.ca),
            roots(&self.ca),
            self.server_leaf.pin,
            SessionLimits::default(),
            Instant::now(),
            SERVER_ADDR,
            "localhost",
        )
        .expect("cliente de sesión")
    }
}

// ---------------------------------------------------------------------------
// Enlace en memoria: el test elige la IP de origen que ve el servidor.
// ---------------------------------------------------------------------------

fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 77, 0, last)), port)
}

/// Una ronda completa: cada cliente entrega lo pendiente al servidor y el
/// servidor enruta cada datagrama a la dirección de destino que le toca.
fn round(server: &mut SessionServer, clients: &mut [(SocketAddr, SessionClient)], now: Instant) {
    for (source, client) in clients.iter_mut() {
        while let Some((_destination, payload)) = client.poll_transmit(now) {
            server
                .handle_datagram(now, *source, &payload)
                .expect("el servidor acepta o rechaza el datagrama");
        }
    }
    while let Some((destination, payload)) = server.poll_transmit(now) {
        if let Some((_, client)) = clients
            .iter_mut()
            .find(|(source, _)| *source == destination)
        {
            client
                .handle_datagram(now, SERVER_ADDR, &payload)
                .expect("el cliente procesa el datagrama del servidor");
        }
    }
}

fn drive(server: &mut SessionServer, clients: &mut [(SocketAddr, SessionClient)]) {
    let now = Instant::now();
    for _ in 0..ROUNDS {
        round(server, clients, now);
    }
}

/// Cuenta sesiones autorizadas y rechazos por cuota de identidad.
fn outcomes(server: &mut SessionServer) -> (usize, usize) {
    let mut authorized = 0;
    let mut quota = 0;
    while let Some(event) = server.next_event() {
        match event {
            ServerEvent::Authorized { .. } => authorized += 1,
            ServerEvent::Rejected {
                reason: RejectReason::IdentityQuota,
            } => quota += 1,
            _ => {}
        }
    }
    (authorized, quota)
}

fn closed(client: &mut SessionClient) -> bool {
    let mut closed = false;
    while let Some(event) = client.next_event() {
        if matches!(event, ClientEvent::Closed) {
            closed = true;
        }
    }
    closed
}

// ---------------------------------------------------------------------------
// Casos.
// ---------------------------------------------------------------------------

/// El quinto handshake desde la misma IP recibe `CONNECTION_REFUSED`; los
/// cuatro primeros se autorizan con el límite por defecto. Cada cliente usa una
/// identidad distinta para aislar el tope por IP de la cuota por identidad.
#[test]
fn per_ip_cap_refuses_the_fifth_connection_from_one_ip() {
    let fixture = Fixture::new();
    let leaves: Vec<TestLeaf> = (0..=MAX_SESSIONS_PER_IP)
        .map(|index| fixture.leaf(&format!("openrelay-client-f9e-ip-{index}")))
        .collect();
    let pins: Vec<SpkiPin> = leaves.iter().map(|leaf| leaf.pin).collect();
    let mut server = fixture.server_allowing(&pins);
    let mut clients: Vec<(SocketAddr, SessionClient)> = leaves
        .iter()
        .enumerate()
        .map(|(index, leaf)| {
            let port = 50_001 + u16::try_from(index).expect("índice acotado");
            (addr(1, port), fixture.client_for(leaf))
        })
        .collect();

    drive(&mut server, &mut clients);

    let (authorized, quota) = outcomes(&mut server);
    assert_eq!(
        authorized, MAX_SESSIONS_PER_IP,
        "sólo el tope por IP queda autorizado"
    );
    assert_eq!(quota, 0, "las identidades distintas no agotan la cuota");
    let refused = &mut clients[MAX_SESSIONS_PER_IP].1;
    assert!(
        closed(refused),
        "el quinto peer observa el cierre por CONNECTION_REFUSED"
    );
    assert!(!refused.is_authorized(), "el quinto peer no obtiene sesión");
}

/// El tope es por IP: cuatro conexiones desde una IP no bloquean una quinta
/// desde otra dirección.
#[test]
fn per_ip_cap_does_not_apply_across_ips() {
    let fixture = Fixture::new();
    let leaves: Vec<TestLeaf> = (0..=MAX_SESSIONS_PER_IP)
        .map(|index| fixture.leaf(&format!("openrelay-client-f9e-across-{index}")))
        .collect();
    let pins: Vec<SpkiPin> = leaves.iter().map(|leaf| leaf.pin).collect();
    let mut server = fixture.server_allowing(&pins);
    let mut clients: Vec<(SocketAddr, SessionClient)> = leaves
        .iter()
        .take(MAX_SESSIONS_PER_IP)
        .enumerate()
        .map(|(index, leaf)| {
            let port = 51_001 + u16::try_from(index).expect("índice acotado");
            (addr(2, port), fixture.client_for(leaf))
        })
        .collect();
    clients.push((
        addr(3, 51_100),
        fixture.client_for(&leaves[MAX_SESSIONS_PER_IP]),
    ));

    drive(&mut server, &mut clients);

    let (authorized, quota) = outcomes(&mut server);
    assert_eq!(
        authorized,
        MAX_SESSIONS_PER_IP + 1,
        "la IP distinta conserva su propio presupuesto"
    );
    assert_eq!(
        quota, 0,
        "sin identidades repetidas no hay rechazo por cuota"
    );
}

/// Un rechazo por tope de IP no gasta un intento de handshake: la IP conserva
/// el presupuesto que no usó.
#[test]
fn per_ip_cap_refusal_does_not_spend_a_handshake_attempt() {
    let fixture = Fixture::new();
    let leaves: Vec<TestLeaf> = (0..MAX_SESSIONS_PER_IP + 3)
        .map(|index| fixture.leaf(&format!("openrelay-client-f9e-attempt-{index}")))
        .collect();
    let pins: Vec<SpkiPin> = leaves.iter().map(|leaf| leaf.pin).collect();
    let mut server = fixture.server_allowing(&pins);
    let mut clients: Vec<(SocketAddr, SessionClient)> = leaves
        .iter()
        .enumerate()
        .map(|(index, leaf)| {
            let port = 52_001 + u16::try_from(index).expect("índice acotado");
            (addr(1, port), fixture.client_for(leaf))
        })
        .collect();

    drive(&mut server, &mut clients);

    let (authorized, quota) = outcomes(&mut server);
    assert_eq!(
        authorized, MAX_SESSIONS_PER_IP,
        "los cuatro primeros peers quedan autorizados"
    );
    assert_eq!(quota, 0, "las identidades distintas no agotan la cuota");
    let ip = IpAddr::V4(Ipv4Addr::new(10, 77, 0, 1));
    assert_eq!(
        server.handshakes.attempts_for(ip),
        u32::try_from(MAX_SESSIONS_PER_IP).expect("tope acotado"),
        "los rechazos por tope de IP no consumen intentos"
    );
}

/// La tercera sesión concurrente de una misma identidad se rechaza; las dos
/// primeras quedan autorizadas.
#[test]
fn identity_cap_refuses_the_third_session_of_one_identity() {
    let fixture = Fixture::new();
    let mut server = fixture.server();
    let mut clients: Vec<(SocketAddr, SessionClient)> = vec![
        (addr(4, 53_001), fixture.client()),
        (addr(5, 53_002), fixture.client()),
        (addr(6, 53_003), fixture.client()),
    ];

    drive(&mut server, &mut clients);

    let (authorized, quota) = outcomes(&mut server);
    assert_eq!(
        authorized, MAX_SESSIONS_PER_IDENTITY,
        "sólo dos sesiones de la identidad quedan autorizadas"
    );
    assert_eq!(quota, 1, "la tercera sesión se rechaza por cuota");
    let mut closed_count = 0;
    for (_, client) in clients.iter_mut() {
        if closed(client) {
            closed_count += 1;
            assert!(!client.is_authorized(), "la sesión sobrante no queda viva");
        }
    }
    assert_eq!(closed_count, 1, "sólo la sesión sobrante observa el cierre");
}

/// La cuota es por identidad: dos sesiones de una identidad no bloquean una
/// tercera de otra identidad distinta.
#[test]
fn identity_cap_does_not_apply_to_a_distinct_identity() {
    let fixture = Fixture::new();
    let other = fixture.leaf("openrelay-client-f9e-other");
    let mut server = fixture.server_allowing(&[fixture.client_leaf.pin, other.pin]);
    let mut clients: Vec<(SocketAddr, SessionClient)> = vec![
        (addr(7, 54_001), fixture.client()),
        (addr(8, 54_002), fixture.client()),
        (addr(9, 54_003), fixture.client_for(&other)),
    ];

    drive(&mut server, &mut clients);

    let (authorized, quota) = outcomes(&mut server);
    assert_eq!(
        authorized,
        MAX_SESSIONS_PER_IDENTITY + 1,
        "la identidad distinta conserva su propio presupuesto"
    );
    assert_eq!(
        quota, 0,
        "no hay rechazo por cuota entre identidades distintas"
    );
    assert!(
        clients.iter_mut().all(|(_, client)| client.is_authorized()),
        "ningún peer queda cerrado por la cuota ajena"
    );
}
