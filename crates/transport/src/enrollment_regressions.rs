//! Regresiones test-only de la frontera de enrolamiento: FIN truncado y Reset
//! parcial sobre QUIC real (sin lógica de producción).
//!
//! Los casos negativos exigen cierre fail-closed (`Rejected(Malformed)`) y
//! limpieza del buffer: conservar el prefijo parcial al recibir FIN no es una
//! semántica aceptable. El caso `Blocked` sin FIN sí debe conservar los bytes.
//!
//! Fixtures nuevas test-only (CA local, hoja ECDSA P-256 y enlace en memoria);
//! no se importa ni modifica `tests/common`.

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn_proto::{Dir, VarInt};
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
use rustls::pki_types::{CertificateDer, SubjectPublicKeyInfoDer};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};
use serde_json::json;

use super::{
    EnrollClient, EnrollReject, EnrollServer, EnrollmentEvent, EnrollmentToken, ENROLL_VERSION,
};
use crate::identity::{self, SpkiPin};

const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44341);
const CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44342);

/// Firma ECDSA P-256/SHA-256 con clave efímera de test.
#[derive(Debug)]
struct TestSigner {
    key: Arc<EcdsaKeyPair>,
    spki: Vec<u8>,
}

impl TestSigner {
    fn new(key_pair: &KeyPair) -> Self {
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
        }
    }
}

impl SigningKey for TestSigner {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if !offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256) {
            return None;
        }
        Some(Box::new(TestSignerHandle {
            key: Arc::clone(&self.key),
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
struct TestSignerHandle {
    key: Arc<EcdsaKeyPair>,
}

impl Signer for TestSignerHandle {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let rng = ring::rand::SystemRandom::new();
        let signature = self
            .key
            .sign(&rng, message)
            .map_err(|_| rustls::Error::General("firma de test falló".into()))?;
        Ok(signature.as_ref().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

struct TestCa {
    cert: Certificate,
    key: KeyPair,
}

impl TestCa {
    fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params CA");
        params
            .distinguished_name
            .push(DnType::CommonName, "openrelay-enroll-regression-ca");
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

    fn leaf(&self, common_name: &str) -> TestLeaf {
        let mut params =
            CertificateParams::new(vec!["localhost".to_string()]).expect("params hoja");
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before = date_time_ymd(1975, 1, 1);
        params.not_after = date_time_ymd(4096, 1, 1);
        let key = KeyPair::generate().expect("clave hoja");
        let cert = params
            .signed_by(&key, &self.cert, &self.key)
            .expect("hoja firmada por la CA");
        let pin = SpkiPin::from_certificate_der(cert.der()).expect("SPKI de la hoja");
        TestLeaf {
            cert,
            signer: Arc::new(TestSigner::new(&key)),
            pin,
        }
    }

    fn cert_der(&self) -> CertificateDer<'static> {
        self.cert.der().clone()
    }
}

struct TestLeaf {
    cert: Certificate,
    signer: Arc<TestSigner>,
    pin: SpkiPin,
}

impl TestLeaf {
    fn certified(&self, ca: &TestCa) -> Arc<CertifiedKey> {
        Arc::new(
            identity::certified_key(
                vec![self.cert.der().clone(), ca.cert_der()],
                self.signer.clone(),
            )
            .expect("CertifiedKey de test"),
        )
    }
}

fn roots(ca: &TestCa) -> Arc<rustls::RootCertStore> {
    identity::root_store([ca.cert_der()]).expect("root store de test")
}

fn window_token() -> EnrollmentToken {
    EnrollmentToken::generate().expect("token del sistema")
}

/// Peer sans-IO bombeado por el enlace en memoria.
trait WirePeer {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)>;
    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String>;
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

fn drain<A: WirePeer, B: WirePeer>(
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

fn exchange<A: WirePeer, B: WirePeer>(
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

/// Handshake de enrolamiento real: pin SPKI + ALPN `openrelay-enroll/1`, sin
/// certificado de cliente (permitido por diseño en esta superficie).
fn setup(ca: &TestCa, token: EnrollmentToken) -> Result<(EnrollClient, EnrollServer), String> {
    let leaf = ca.leaf("openrelay-enroll-regression");
    let mut server = EnrollServer::new(leaf.certified(ca)).map_err(|error| error.to_string())?;
    server
        .open_window(token, Duration::from_secs(120), Instant::now())
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

fn collect_server_events(server: &mut EnrollServer) -> Vec<EnrollmentEvent> {
    let mut events = Vec::new();
    while let Some(event) = server.next_event() {
        events.push(event);
    }
    events
}

fn collect_client_events(client: &mut EnrollClient) -> Vec<EnrollmentEvent> {
    let mut events = Vec::new();
    while let Some(event) = client.next_event() {
        events.push(event);
    }
    events
}

fn has_reject(events: &[EnrollmentEvent], expected: EnrollReject) -> bool {
    events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Rejected(reason) if *reason == expected))
}

fn has_accepted(events: &[EnrollmentEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Accepted(_)))
}

/// Frame `enroll.request` idéntico al que escribe `send_token`.
fn client_spki_der() -> Vec<u8> {
    rcgen::KeyPair::generate()
        .expect("clave cliente")
        .public_key_der()
}

fn request_frame(token: &EnrollmentToken) -> Vec<u8> {
    let payload = serde_json::to_vec(&json!({
        "type": "enroll.request",
        "version": ENROLL_VERSION,
        "token": token.to_hex(),
        "spki": super::to_hex(&client_spki_der()),
    }))
    .expect("JSON del contrato");
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// Frame `enroll.result` accepted idéntico al que responde el servidor.
fn result_frame() -> Vec<u8> {
    let payload = serde_json::to_vec(&json!({
        "type": "enroll.result",
        "version": ENROLL_VERSION,
        "status": "accepted",
    }))
    .expect("JSON del contrato");
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

#[derive(Clone, Copy)]
enum Ending {
    Fin,
    Reset,
    Open,
}

fn write_all(client: &mut EnrollClient, stream: quinn_proto::StreamId, bytes: &[u8]) {
    let mut written = 0usize;
    while written < bytes.len() {
        match client.conn.send_stream(stream).write(&bytes[written..]) {
            Ok(0) | Err(_) => panic!("escritura rechazada en el stream de prueba"),
            Ok(count) => written += count,
        }
    }
}

/// El cliente abre el stream de enrolamiento y envía `bytes` tal cual.
fn client_write_raw(
    client: &mut EnrollClient,
    bytes: &[u8],
    ending: Ending,
) -> quinn_proto::StreamId {
    let stream = client
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidi disponible");
    write_all(client, stream, bytes);
    match ending {
        Ending::Fin => client
            .conn
            .send_stream(stream)
            .finish()
            .expect("FIN del cliente"),
        Ending::Reset => client
            .conn
            .send_stream(stream)
            .reset(VarInt::from_u32(0))
            .expect("RESET del cliente"),
        Ending::Open => {}
    }
    stream
}

/// El servidor escribe sobre el stream bidi recibido del cliente.
fn server_write_raw(server: &mut EnrollServer, bytes: &[u8], ending: Ending) {
    let stream = server.recv_stream.expect("stream del cliente aceptado");
    let conn = server.conn.as_mut().expect("conexión del servidor");
    let mut written = 0usize;
    while written < bytes.len() {
        match conn.send_stream(stream).write(&bytes[written..]) {
            Ok(0) | Err(_) => panic!("escritura rechazada en el stream del servidor"),
            Ok(count) => written += count,
        }
    }
    match ending {
        Ending::Fin => conn.send_stream(stream).finish().expect("FIN del servidor"),
        Ending::Reset => conn
            .send_stream(stream)
            .reset(VarInt::from_u32(0))
            .expect("RESET del servidor"),
        Ending::Open => {}
    }
}

fn ready_pair(ca: &TestCa) -> (EnrollClient, EnrollServer, EnrollmentToken) {
    let window = window_token();
    let (mut client, mut server) = setup(ca, window.clone()).expect("configuración de enroll");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake enroll");
    assert!(client.is_ready(), "cliente listo tras validar pin y ALPN");
    assert!(server.is_ready(), "servidor listo sin cert de cliente");
    (client, server, window)
}

#[test]
fn server_accepts_complete_request_frame_with_fin_control() {
    // Control del arnés: demuestra que un frame completo terminado con FIN real
    // llega y se procesa, de modo que el RED de los casos truncados no es un
    // artefacto de entrega del enlace en memoria.
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    let frame = request_frame(&window);

    client_write_raw(&mut client, &frame, Ending::Fin);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("frame completo");

    let events = collect_server_events(&mut server);
    assert_eq!(server.attempts(), 1, "un enroll.request completo");
    assert!(
        has_accepted(&events),
        "un frame completo con FIN debe aceptarse, vistos {events:?}"
    );
    assert!(!has_reject(&events, EnrollReject::Malformed));
    assert!(
        server.rx.is_empty(),
        "el frame consumido deja el buffer limpio"
    );
}

#[test]
fn server_fails_closed_on_prefix_fin_of_1_to_3_bytes() {
    for prefix_len in 1..=3usize {
        let ca = TestCa::new();
        let (mut client, mut server, window) = ready_pair(&ca);
        let frame = request_frame(&window);
        assert!(frame.len() > prefix_len);

        client_write_raw(&mut client, &frame[..prefix_len], Ending::Fin);
        exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("prefijo en vuelo");

        let events = collect_server_events(&mut server);
        assert!(
            server.closed,
            "FIN con prefijo de {prefix_len} byte(s) debe cerrar la frontera"
        );
        assert!(
            has_reject(&events, EnrollReject::Malformed),
            "FIN truncado de {prefix_len} byte(s) debe registrarse como malformado, vistos {events:?}"
        );
        assert!(
            !has_accepted(&events),
            "un prefijo truncado no puede aceptar el enrolamiento"
        );
        assert!(
            server.rx.is_empty(),
            "tras FIN truncado no debe conservarse el prefijo parcial"
        );
        assert_eq!(server.attempts(), 0, "no hubo mensaje completo");
        assert!(server.window.is_some(), "la ventana no se consumió");
    }
}

#[test]
fn server_fails_closed_on_valid_length_prefix_with_truncated_body_and_fin() {
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    let frame = request_frame(&window);
    // Prefijo de longitud completo + cuerpo truncado (un byte del JSON).
    let cut = 5usize;
    assert!(frame.len() > cut);

    client_write_raw(&mut client, &frame[..cut], Ending::Fin);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("cuerpo truncado");

    let events = collect_server_events(&mut server);
    assert!(
        server.closed,
        "longitud declarada con cuerpo truncado y FIN debe cerrar la frontera"
    );
    assert!(
        has_reject(&events, EnrollReject::Malformed),
        "cuerpo truncado debe registrarse como malformado, vistos {events:?}"
    );
    assert!(!has_accepted(&events));
    assert!(
        server.rx.is_empty(),
        "tras FIN truncado no debe conservarse el prefijo parcial"
    );
    assert_eq!(server.attempts(), 0, "no hubo enroll.request completo");
}

#[test]
fn server_fails_closed_on_partial_reset() {
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    let frame = request_frame(&window);
    let cut = 5usize;

    client_write_raw(&mut client, &frame[..cut], Ending::Reset);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("reset parcial");

    let events = collect_server_events(&mut server);
    assert!(server.closed, "un Reset parcial debe cerrar la frontera");
    assert!(
        has_reject(&events, EnrollReject::Malformed),
        "Reset parcial debe registrarse como malformado, vistos {events:?}"
    );
    assert!(!has_accepted(&events));
    assert!(server.rx.is_empty());
}

#[test]
fn server_preserves_bytes_when_blocked_without_fin() {
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    let frame = request_frame(&window);
    let prefix_len = 3usize;

    // Sin FIN: el prefijo queda en vuelo y el servidor debe conservarlo.
    client_write_raw(&mut client, &frame[..prefix_len], Ending::Open);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("prefijo en vuelo");
    let events = collect_server_events(&mut server);
    assert!(!server.closed, "sin terminar el stream no hay cierre");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EnrollmentEvent::Rejected(_))),
        "sin FIN no hay rechazo, vistos {events:?}"
    );
    assert!(
        !server.rx.is_empty(),
        "los bytes recibidos sin FIN deben conservarse como prefijo"
    );
}

#[test]
fn client_fails_closed_on_truncated_result_and_fin() {
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    // El cliente abre el stream con un prefijo incompleto: el servidor acepta el
    // stream pero no responde (no hay mensaje completo), así que es el propio
    // servidor quien escribe la respuesta truncada en el mismo stream bidi.
    let frame = request_frame(&window);
    client_write_raw(&mut client, &frame[..3], Ending::Open);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("stream abierto");
    assert!(server.recv_stream.is_some(), "stream del cliente aceptado");
    assert_eq!(server.attempts(), 0, "sin mensaje completo");

    let result = result_frame();
    let cut = 5usize;
    assert!(result.len() > cut);
    server_write_raw(&mut server, &result[..cut], Ending::Fin);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("respuesta truncada");

    let events = collect_client_events(&mut client);
    assert!(
        has_reject(&events, EnrollReject::Malformed),
        "respuesta truncada con FIN debe registrarse como malformada, vistos {events:?}"
    );
    assert!(
        !has_accepted(&events),
        "no puede aceptarse una respuesta truncada"
    );
    assert!(
        client.rx.is_empty(),
        "el cliente no debe conservar el prefijo parcial de la respuesta"
    );
}

/// Test E: tras una respuesta `enroll.result` truncada con FIN, el cliente no
/// sólo registra `Rejected(Malformed)`: falta la terminalidad real de la
/// conexión. La lane de fix no puede limitarse a un `Rejected` local + buffer
/// vacío dejando la conexión viva.
#[test]
fn client_is_terminal_after_truncated_result_and_fin() {
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    let frame = request_frame(&window);
    client_write_raw(&mut client, &frame[..3], Ending::Open);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("stream abierto");
    assert!(server.recv_stream.is_some(), "stream del cliente aceptado");

    let result = result_frame();
    let cut = 5usize;
    assert!(result.len() > cut);
    server_write_raw(&mut server, &result[..cut], Ending::Fin);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("respuesta truncada");

    let events = collect_client_events(&mut client);
    // RED de partida: el FIN truncado se registra una sola vez como malformado.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EnrollmentEvent::Rejected(EnrollReject::Malformed)))
            .count(),
        1,
        "el FIN truncado debe registrarse exactamente una vez, vistos {events:?}"
    );
    assert!(
        !has_accepted(&events),
        "no puede aceptarse una respuesta truncada"
    );
    assert!(client.rx.is_empty(), "el prefijo parcial no se conserva");

    // Terminalidad observada en la conexión QUIC real del cliente tras bombear
    // la respuesta (incluye el datagrama de FIN y su acknowledgment).
    assert!(
        client.conn.is_closed(),
        "el cliente debe cerrar su conexión tras una respuesta truncada"
    );

    // Terminalidad observable por el API público: no acepta más estado.
    assert!(!client.is_ready(), "un cliente terminal no está listo");
    assert!(
        matches!(
            client.send_token(&window, &client_spki_der()),
            Err(super::EnrollError::NotReady)
        ),
        "un cliente terminal debe rechazar send_token con NotReady"
    );

    // El cierre debe ser real en la red, no sólo un flag local: el servidor
    // tiene que observar la conexión cerrada. Se bombea hasta 16 rondas para
    // dar salida al CONNECTION_CLOSE pendiente.
    let mut peer_saw_close = false;
    for _ in 0..16 {
        exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 2)
            .expect("bombeo del cierre al peer");
        // El servidor sólo marca `closed` al consumir su `ConnectionLost`, que
        // `pump` procesa dentro de `handle_datagram`; `poll_transmit` fuerza el
        // sondeo del estado de la conexión.
        let _ = server.poll_transmit(Instant::now());
        if server.closed {
            peer_saw_close = true;
            break;
        }
    }
    assert!(
        peer_saw_close,
        "el par debe observar el cierre del cliente, no un flag local"
    );

    // No debe quedar ninguna aceptación diferida en la cola de eventos.
    assert!(
        !has_accepted(&collect_client_events(&mut client)),
        "ningún Accepted puede aparecer después del truncado"
    );
}

/// Control del Test E: respuesta completa con FIN sobre el mismo stream bidi
/// termina en `Accepted`, de modo que la terminalidad exigida arriba no es un
/// artefacto del arnés.
#[test]
fn client_accepts_complete_result_with_fin_control() {
    let ca = TestCa::new();
    let (mut client, mut server, window) = ready_pair(&ca);
    // Frame parcial escrito a mano: no pasa por `send_token`, así que fijamos el
    // pin que ese frame declararía. Sólo importa el framing de la respuesta.
    client.sent_identity = Some(SpkiPin::from_spki_der(&client_spki_der()));
    let frame = request_frame(&window);
    client_write_raw(&mut client, &frame[..3], Ending::Open);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("stream abierto");

    server_write_raw(&mut server, &result_frame(), Ending::Fin);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("respuesta completa");

    let events = collect_client_events(&mut client);
    assert!(
        has_accepted(&events),
        "una respuesta completa con FIN debe aceptarse, vistos {events:?}"
    );
    assert!(!has_reject(&events, EnrollReject::Malformed));
    assert!(
        client.rx.is_empty(),
        "el frame consumido deja el buffer limpio"
    );
}
