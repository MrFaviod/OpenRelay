//! Regresiones de seguridad 0-RTT/reanudación sobre QUIC real.
//!
//! El servidor de producción fija `send_tls13_tickets = 0` y
//! `max_early_data_size = 0`, y el cliente fija `enable_early_data = false`.
//! Estos casos no se conforman con mirar `has_0rtt()` del handshake normal:
//! construyen un cliente **dispuesto** a early data (rustls con
//! `enable_early_data = true` y un `ClientSessionStore` propio que cuenta
//! tickets) contra el `SessionServer` real, y miden dos cosas:
//!
//! 1. cuántos tickets TLS 1.3 entrega el servidor (esperado: cero), y
//! 2. si esa disposición basta para ofrecer 0-RTT en una reconexión con el
//!    mismo store (esperado: no, porque no hay material de reanudación).
//!
//! El store contador es además el control de no vacuidad: si rustls no lo
//! consultara, `take_tls13_ticket` sería cero y la ausencia de tickets no
//! probaría nada.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use openrelay_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};
use openrelay_proto::wire::ControlMessage;
use quinn_proto::crypto::rustls::QuicClientConfig;
use quinn_proto::{
    ClientConfig as QuinnClientConfig, Connection, ConnectionHandle, DatagramEvent, Endpoint,
    EndpointConfig, TransportConfig,
};
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::client::{ClientSessionStore, Resumption};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig as TlsClientConfig, NamedGroup, RootCertStore};

use super::{
    Acl, Capabilities, ServerEvent, SessionClient, SessionId, SessionLimits, SessionServer,
    TransportError,
};
use crate::identity::{self, PinnedServerVerifier, SpkiPin, StaticClientCertResolver};
use crate::ALPN_SESSION;

const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44331);
const CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44332);

// ---------------------------------------------------------------------------
// Fixtures test-only: CA local y hojas ECDSA P-256 en memoria.
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
            .push(DnType::CommonName, "openrelay-test-ca-t10b");
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
            .expect("hoja firmada");
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
        let signer = any_ecdsa_type(&PrivateKeyDer::Pkcs8(pkcs8)).expect("signer de test");
        Arc::new(
            identity::certified_key(vec![self.der(), ca.cert_der()], signer)
                .expect("CertifiedKey de test"),
        )
    }
}

fn roots(ca: &TestCa) -> Arc<RootCertStore> {
    let mut store = RootCertStore::empty();
    store.add(ca.cert_der()).expect("ancla de test");
    Arc::new(store)
}

fn full_capabilities() -> Capabilities {
    Capabilities::none()
        .with_mic()
        .with_control_read()
        .with_control_write()
        .with_return_audio()
}

/// Servidor real de la crate, con ACL explícita para la hoja de cliente.
fn server_for(server_leaf: &TestLeaf, client_leaf: &TestLeaf, ca: &TestCa) -> SessionServer {
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, full_capabilities());
    SessionServer::new(
        server_leaf.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión")
}

// ---------------------------------------------------------------------------
// Store de sesiones contador.
// ---------------------------------------------------------------------------

/// `ClientSessionStore` que no guarda nada y cuenta lo que rustls le pide.
///
/// `insert_tls13_ticket` es la única vía por la que un servidor entrega
/// material de reanudación: si el servidor no envía tickets, el contador queda
/// en cero y una reconexión no puede ofrecer 0-RTT. `take_tls13_ticket` cuenta
/// las consultas para probar que el store se usa de verdad.
#[derive(Debug, Default)]
struct CountingStore {
    inserted: AtomicUsize,
    taken: AtomicUsize,
    kx_hints: AtomicUsize,
}

impl CountingStore {
    fn inserted(&self) -> usize {
        self.inserted.load(Ordering::SeqCst)
    }

    fn taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }
}

impl ClientSessionStore for CountingStore {
    fn set_kx_hint(&self, _server_name: ServerName<'static>, _group: NamedGroup) {
        self.kx_hints.fetch_add(1, Ordering::SeqCst);
    }

    fn kx_hint(&self, _server_name: &ServerName<'_>) -> Option<NamedGroup> {
        None
    }

    fn set_tls12_session(
        &self,
        _server_name: ServerName<'static>,
        _value: rustls::client::Tls12ClientSessionValue,
    ) {
    }

    fn tls12_session(
        &self,
        _server_name: &ServerName<'_>,
    ) -> Option<rustls::client::Tls12ClientSessionValue> {
        None
    }

    fn remove_tls12_session(&self, _server_name: &ServerName<'static>) {}

    fn insert_tls13_ticket(
        &self,
        _server_name: ServerName<'static>,
        _value: rustls::client::Tls13ClientSessionValue,
    ) {
        self.inserted.fetch_add(1, Ordering::SeqCst);
    }

    fn take_tls13_ticket(
        &self,
        _server_name: &ServerName<'static>,
    ) -> Option<rustls::client::Tls13ClientSessionValue> {
        self.taken.fetch_add(1, Ordering::SeqCst);
        None
    }
}

// ---------------------------------------------------------------------------
// Peer QUIC con early data habilitada.
// ---------------------------------------------------------------------------

/// Cliente QUIC crudo: mismo TLS/mTLS/ALPN/pin que el cliente real, pero con
/// `enable_early_data = true` y el store contador inyectado. Es el intento
/// adversarial que la superficie de sesión no permite construir.
struct EarlyDataPeer {
    endpoint: Endpoint,
    conn: Connection,
    handle: ConnectionHandle,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
    scratch: Vec<u8>,
}

impl EarlyDataPeer {
    fn connect(
        client_leaf: &TestLeaf,
        server_leaf: &TestLeaf,
        ca: &TestCa,
        store: Arc<CountingStore>,
        now: Instant,
    ) -> Self {
        let verifier =
            PinnedServerVerifier::new(roots(ca), server_leaf.pin).expect("verificador pinneado");
        let mut tls = TlsClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_cert_resolver(Arc::new(StaticClientCertResolver::new(
                client_leaf.certified(ca),
            )));
        tls.alpn_protocols = vec![ALPN_SESSION.to_vec()];
        // Disposición explícita a early data: si hubiera ticket, rustls lo usaría.
        tls.enable_early_data = true;
        tls.resumption = Resumption::store(store);
        let crypto = QuicClientConfig::try_from(tls).expect("crypto de cliente");
        let mut quinn = QuinnClientConfig::new(Arc::new(crypto));
        let mut transport = TransportConfig::default();
        transport.datagram_receive_buffer_size(Some(4_096));
        transport.datagram_send_buffer_size(4_096);
        quinn.transport_config(Arc::new(transport));
        let mut endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None);
        let (handle, conn) = endpoint
            .connect(now, quinn, SERVER_ADDR, "localhost")
            .expect("conexión cruda");
        Self {
            endpoint,
            conn,
            handle,
            outgoing: VecDeque::new(),
            scratch: Vec::new(),
        }
    }

    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        if let Some(item) = self.outgoing.pop_front() {
            return Some(item);
        }
        self.scratch.clear();
        let transmit = self.conn.poll_transmit(now, 1, &mut self.scratch)?;
        Some((transmit.destination, self.scratch[..transmit.size].to_vec()))
    }

    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        let mut response = Vec::new();
        let event = self.endpoint.handle(
            now,
            from,
            None,
            None,
            BytesMut::from(payload),
            &mut response,
        );
        match event {
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if handle == self.handle {
                    self.conn.handle_event(event);
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                self.outgoing
                    .push_back((transmit.destination, response[..transmit.size].to_vec()));
            }
            _ => {}
        }
        self.observe();
        Ok(())
    }

    fn observe(&mut self) {
        while let Some(event) = self.conn.poll_endpoint_events() {
            if let Some(event) = self.endpoint.handle_event(self.handle, event) {
                self.conn.handle_event(event);
            }
        }
        while self.conn.poll().is_some() {}
    }
}

/// Tipos de paquete de cabecera larga de un datagrama, en orden de
/// coalescencia. `0` Initial, `1` 0-RTT, `2` Handshake, `3` Retry.
///
/// Recorre sólo lo que puede parsear sin ambigüedad: si la longitud deja de
/// ser consistente se detiene y devuelve lo leído hasta ahí.
fn long_header_types(datagram: &[u8]) -> Vec<u8> {
    fn varint(input: &[u8], offset: &mut usize) -> Option<u64> {
        let first = *input.get(*offset)?;
        let len = 1usize << (first >> 6);
        let mut value = u64::from(first & 0x3f);
        for index in 1..len {
            value = (value << 8) | u64::from(*input.get(*offset + index)?);
        }
        *offset += len;
        Some(value)
    }

    let mut types = Vec::new();
    let mut offset = 0usize;
    while offset < datagram.len() {
        let first = datagram[offset];
        if first & 0x80 == 0 {
            break; // 1-RTT o basura: no es cabecera larga.
        }
        types.push((first & 0x30) >> 4);
        offset += 1;
        if datagram.len() < offset + 5 {
            break;
        }
        offset += 4; // versión
        let Some(dcid_len) = datagram.get(offset).copied() else {
            break;
        };
        offset += 1 + usize::from(dcid_len);
        let Some(scid_len) = datagram.get(offset).copied() else {
            break;
        };
        offset += 1 + usize::from(scid_len);
        if types.last() == Some(&0) {
            // Initial: token con longitud variable.
            let Some(token_len) = varint(datagram, &mut offset) else {
                break;
            };
            offset += token_len as usize;
        }
        let Some(length) = varint(datagram, &mut offset) else {
            break;
        };
        offset += length as usize;
    }
    types
}

/// Bombeo de datagramas entre dos extremos: `poll` produce el siguiente
/// datagrama de un extremo y `deliver` lo entrega al otro.
type TransmitPoll<A> = fn(&mut A, Instant) -> Option<(SocketAddr, Vec<u8>)>;
type DatagramDeliver<B> = fn(&mut B, Instant, SocketAddr, &[u8]) -> Result<(), TransportError>;

fn drain<A, B>(
    from: &mut A,
    from_addr: SocketAddr,
    to: &mut B,
    now: Instant,
    poll: TransmitPoll<A>,
    deliver: DatagramDeliver<B>,
) -> Result<(), TransportError> {
    for _ in 0..256 {
        let Some((_destination, payload)) = poll(from, now) else {
            return Ok(());
        };
        deliver(to, now, from_addr, &payload)?;
    }
    Err(TransportError::Config("bombeo de datagramas no termina"))
}

fn exchange(
    peer: &mut EarlyDataPeer,
    server: &mut SessionServer,
    now: Instant,
    rounds: usize,
) -> Result<(), TransportError> {
    for _ in 0..rounds {
        drain(
            peer,
            CLIENT_ADDR,
            server,
            now,
            EarlyDataPeer::poll_transmit,
            |server, now, from, payload| server.handle_datagram(now, from, payload),
        )?;
        drain(
            server,
            SERVER_ADDR,
            peer,
            now,
            SessionServer::poll_transmit,
            EarlyDataPeer::handle_datagram,
        )?;
    }
    Ok(())
}

fn authorized_session(server: &mut SessionServer) -> Option<super::SessionId> {
    while let Some(event) = server.next_event() {
        if let ServerEvent::Authorized { session, .. } = event {
            return Some(session);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Casos.
// ---------------------------------------------------------------------------

/// Caso 1: contra el `SessionServer` real, un cliente dispuesto a early data
/// no recibe ni un ticket TLS 1.3, y su store se consulta de verdad.
///
/// El handshake se completa y la sesión se autoriza (control positivo): la
/// ausencia de tickets no se explica por un handshake roto.
#[test]
fn production_server_sends_no_session_ticket_to_early_data_client() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon-t10b-tickets", true);
    let client_leaf = ca.leaf("openrelay-client-t10b-tickets", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let store = Arc::new(CountingStore::default());
    let mut peer = EarlyDataPeer::connect(
        &client_leaf,
        &server_leaf,
        &ca,
        store.clone(),
        Instant::now(),
    );

    exchange(&mut peer, &mut server, Instant::now(), 24).expect("handshake mTLS");
    assert!(
        !peer.conn.is_handshaking(),
        "control positivo: el handshake mTLS termina"
    );
    assert!(
        !peer.conn.is_closed(),
        "control positivo: la conexión sigue viva tras autorizar"
    );
    let _session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    assert!(
        store.taken() >= 1,
        "control de no vacuidad: rustls consultó el store al negociar"
    );
    assert_eq!(
        store.inserted(),
        0,
        "el servidor de producción no entrega ni un ticket de reanudación"
    );
    assert!(
        !peer.conn.has_0rtt(),
        "sin material de reanudación quinn no habilita 0-RTT ni con early data dispuesta"
    );
    assert!(!peer.conn.accepted_0rtt(), "nada se aceptó como early data");
}

/// Caso 2: reconexión con el mismo store, dispuesta a early data, contra el
/// servidor real. Mide las tres piezas del intento adversarial: el store se
/// consulta, no devuelve ticket, y la primera flight no lleva ningún paquete
/// 0-RTT.
#[test]
fn early_data_client_cannot_offer_0rtt_on_reconnect() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon-t10b-reconnect", true);
    let client_leaf = ca.leaf("openrelay-client-t10b-reconnect", false);
    let store = Arc::new(CountingStore::default());

    // Primera conexión: handshake completo contra el servidor real.
    {
        let mut server = server_for(&server_leaf, &client_leaf, &ca);
        let mut peer = EarlyDataPeer::connect(
            &client_leaf,
            &server_leaf,
            &ca,
            store.clone(),
            Instant::now(),
        );
        exchange(&mut peer, &mut server, Instant::now(), 24).expect("handshake inicial");
        assert!(!peer.conn.is_handshaking(), "primer handshake completo");
        assert_eq!(
            store.inserted(),
            0,
            "el servidor no dejó ticket reutilizable"
        );
    }

    // Segunda conexión: mismo store, misma disposición a early data.
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = EarlyDataPeer::connect(
        &client_leaf,
        &server_leaf,
        &ca,
        store.clone(),
        Instant::now(),
    );

    // La consulta al store ocurre al construir el ClientHello: si no, el
    // intento no es adversarial, es un no-op.
    assert!(
        store.taken() >= 1,
        "rustls consultó el store al iniciar la reconexión"
    );
    assert!(
        !peer.conn.has_0rtt(),
        "quinn no habilita 0-RTT: no hay ticket que lo permita"
    );

    // Primera flight observada en el wire: ni un paquete de tipo 0-RTT. Los
    // datagramas se conservan y se entregan al servidor, porque el enlace en
    // memoria no pierde paquetes y descartarlos dejaría el handshake colgado
    // esperando una retransmisión por PTO que este test no dispara.
    let mut first_flight = Vec::new();
    let mut seen = Vec::new();
    for _ in 0..8 {
        let Some((_destination, payload)) = peer.poll_transmit(Instant::now()) else {
            break;
        };
        seen.extend(long_header_types(&payload));
        first_flight.push(payload);
    }
    assert!(
        !seen.is_empty(),
        "control positivo: el cliente emite su primera flight"
    );
    assert!(
        seen.contains(&0),
        "control positivo: la flight arranca con un Initial real"
    );
    assert!(
        !seen.contains(&1),
        "ningún paquete 0-RTT en la primera flight: {seen:?}"
    );

    let now = Instant::now();
    for payload in &first_flight {
        server
            .handle_datagram(now, CLIENT_ADDR, payload)
            .expect("primera flight entregada");
    }
    exchange(&mut peer, &mut server, now, 24).expect("handshake de reconexión");
    assert!(
        !peer.conn.is_handshaking(),
        "la reconexión completa un handshake nuevo, no una reanudación"
    );
    assert!(!peer.conn.accepted_0rtt(), "no se aceptó early data");
}

// ---------------------------------------------------------------------------
// Residuo de capacidades: el ingreso en la cola de aplicación no depende de
// `mic` ni de `control_write`.
// ---------------------------------------------------------------------------
//
// EXCEPCIÓN DELIBERADA Y ACOTADA — lectura privada de `Slot`.
//
// Los getters públicos (`recv_audio`/`recv_control`) filtran por capacidad y
// devuelven `None`, así que por sí solos **no** distinguen «no se encoló nada»
// de «se encoló y la lectura pública lo oculta». Ese es exactamente el residuo
// que estos casos deben medir, y no hay superficie pública que lo exponga. Por
// eso el helper `queued_*` lee `slot.audio` / `slot.control` directamente,
// desde el mismo módulo de test (`#[path]` dentro de `quinn`), sin tocar
// producción.
//
// Límites de la excepción, para que no se lea como más de lo que es:
// - NO es un bypass de entrega: el peer sigue sin recibir el DATAGRAM/stream
//   denegado por la ruta pública, y los `recv_*` se siguen afirmando en `None`.
// - NO es memoria ilimitada: `MAX_QUEUED_AUDIO` y `MAX_QUEUED_CONTROL` son 8 y
//   la política de descarte del más viejo no se relaja.
// - La capacidad es inmutable hoy (no hay ruta pública que la conceda o revoque
//   tras autorizar), así que el residuo no se «simula»: se produce con tráfico
//   QUIC real sobre mTLS y una ACL real, sólo que con una entrada que **no**
//   concede el permiso en cuestión.
// - El requisito probado es «no retener payload de aplicación denegado», no
//   «cero bytes en el wire» ni «cero buffering interno de QUIC o de framing»:
//   un DATAGRAM o frame denegado puede cruzar el wire y llegar a la cola de
//   quinn; lo que se exige es que no quede encolado para la aplicación.

/// Nº de frames de audio encolados en el slot, sin pasar por el filtro de
/// capacidad de `recv_audio`. Ver la excepción documentada arriba.
fn queued_audio(server: &SessionServer, session: SessionId) -> Option<usize> {
    server.slots.get(&session).map(|slot| slot.audio.len())
}

/// Nº de mensajes de control encolados en el slot, sin el filtro de capacidad
/// de `recv_control`. Ver la excepción documentada arriba.
fn queued_control(server: &SessionServer, session: SessionId) -> Option<usize> {
    server.slots.get(&session).map(|slot| slot.control.len())
}

/// Sesión autorizada visible desde ambos extremos, o `None` si el handshake no
/// la concedió: el control positivo se afirma antes de atribuir nada al residuo.
fn authorized_pair(client: &mut SessionClient, server: &mut SessionServer) -> Option<SessionId> {
    let session = authorized_session(server);
    assert!(
        session.is_some(),
        "control positivo: la ACL autoriza la sesión mTLS"
    );
    assert!(
        client.is_authorized(),
        "control positivo: el cliente verificó el pin del servidor"
    );
    session
}

fn frame(seq: u32) -> AudioFrame {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample = (index as i16) - 120 + seq as i16;
    }
    AudioFrame::new(1, seq, Flags::NONE, samples).expect("frame v1")
}

fn ping(request_id: &str) -> ControlMessage {
    ControlMessage::Ping {
        request_id: request_id.to_string(),
    }
}

/// Bombea un vuelo `SessionClient → SessionServer` hasta agotar transmisiones.
fn session_drain(
    from: &mut SessionClient,
    from_addr: SocketAddr,
    to: &mut SessionServer,
    now: Instant,
) -> Result<(), TransportError> {
    for _ in 0..256 {
        let Some((_destination, payload)) = from.poll_transmit(now) else {
            return Ok(());
        };
        to.handle_datagram(now, from_addr, &payload)?;
    }
    Err(TransportError::Connect("bombeo no termina".to_string()))
}

/// Bombea un vuelo `SessionServer → SessionClient` hasta agotar transmisiones.
fn session_drain_back(
    from: &mut SessionServer,
    from_addr: SocketAddr,
    to: &mut SessionClient,
    now: Instant,
) -> Result<(), TransportError> {
    for _ in 0..256 {
        let Some((_destination, payload)) = from.poll_transmit(now) else {
            return Ok(());
        };
        to.handle_datagram(now, from_addr, &payload)?;
    }
    Err(TransportError::Connect("bombeo no termina".to_string()))
}

/// Intercambia vuelos hasta que ninguno de los dos tenga algo que enviar.
fn session_exchange(
    client: &mut SessionClient,
    client_addr: SocketAddr,
    server: &mut SessionServer,
    server_addr: SocketAddr,
    rounds: usize,
) -> Result<(), TransportError> {
    let now = Instant::now();
    for _ in 0..rounds {
        session_drain(client, client_addr, server, now)?;
        session_drain_back(server, server_addr, client, now)?;
    }
    Ok(())
}

/// Servidor con una entrada ACL que concede exactamente `caps`, más el cliente
/// real (`SessionClient`, no el peer crudo de early data).
fn session_pair(
    server_leaf: &TestLeaf,
    client_leaf: &TestLeaf,
    ca: &TestCa,
    caps: Capabilities,
) -> (SessionClient, SessionServer) {
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, caps);
    let server = SessionServer::new(
        server_leaf.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión");
    let client = SessionClient::connect(
        client_leaf.certified(ca),
        roots(ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");
    (client, server)
}

/// Audio de cliente **válido** (frame v1 decodificable) sobre una sesión que
/// sólo concede `return_audio`: el DATAGRAM se recibe, pero sin `mic` no debe
/// quedar retenido en la cola de aplicación del servidor.
///
/// Control positivo en el mismo caso: con `mic` concedido, el **mismo** tráfico
/// sí puebla la cola, así que un cero en el caso denegado no es un `None`
/// trivial (payload roto, sesión no autorizada o bombeo insuficiente).
#[test]
fn valid_audio_without_mic_is_not_queued_for_the_application() {
    // Caso denegado: `return_audio` pero no `mic`.
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon-res-audio", true);
    let client_leaf = ca.leaf("openrelay-client-res-audio", false);
    let (mut client, mut server) = session_pair(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_return_audio(),
    );
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24)
        .expect("handshake mTLS");
    let session = authorized_pair(&mut client, &mut server).expect("sesión autorizada");
    assert_eq!(
        server.capabilities(session),
        Some(Capabilities::none().with_return_audio()),
        "las capacidades del caso denegado son las declaradas, no un fallo de ACL"
    );

    let denied = frame(11);
    let before = client.conn.stats().frame_tx.datagram;
    client.send_audio(&denied).expect("cliente envía DATAGRAM");
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8)
        .expect("audio en vuelo");
    assert!(
        client.conn.stats().frame_tx.datagram > before,
        "control positivo de entrega: el DATAGRAM de audio salió del cliente"
    );
    assert!(
        !client.conn.is_handshaking() && !client.conn.is_closed(),
        "no se cerró la sesión por el audio denegado"
    );
    assert!(
        queued_audio(&server, session) == Some(0),
        "residuo: audio válido sin `mic` no debe quedar en la cola de aplicación"
    );
    assert_eq!(
        server.recv_audio(session),
        None,
        "la lectura pública sigue denegando"
    );
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "la identidad no se revoca por el audio denegado"
    );
    assert_eq!(
        server.capabilities(session),
        Some(Capabilities::none().with_return_audio()),
        "las capacidades no mutan por el audio denegado"
    );

    // Control positivo del mismo tráfico: `mic` concedido sí puebla la cola.
    // Va en su propio runner para observarlo aunque el caso denegado sea RED.
}

/// Control positivo del caso de audio: con `mic` concedido, el **mismo** tráfico
/// que el caso denegado sí puebla la cola de aplicación y `recv_audio` lo
/// entrega. Sin esto, un `Some(0)`/`None` en el caso denegado no distinguiría
/// «no se encoló» de «el bombeo o el payload nunca funcionaron».
#[test]
fn audio_with_mic_is_queued_for_the_application() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon-res-audio-pos", true);
    let client_leaf = ca.leaf("openrelay-client-res-audio-pos", false);
    let (mut client, mut server) = session_pair(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_mic(),
    );
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24)
        .expect("handshake mTLS");
    let session = authorized_pair(&mut client, &mut server).expect("sesión autorizada");
    let granted = frame(12);
    client.send_audio(&granted).expect("cliente envía DATAGRAM");
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8)
        .expect("audio en vuelo");
    assert!(
        queued_audio(&server, session) == Some(1),
        "control positivo: con `mic` el mismo tráfico sí queda encolado una vez"
    );
    assert_eq!(
        server.recv_audio(session),
        Some(granted),
        "control positivo: la lectura pública entrega el frame encolado"
    );
}

/// Control de cliente **válido** sobre una sesión que sólo concede
/// `control_read`: el stream se recibe y se decodifica, pero sin
/// `control_write` no debe quedar retenido en la cola de aplicación.
///
/// Control positivo en el mismo caso: con `control_write` concedido, el mismo
/// mensaje sí puebla la cola y `recv_control` lo entrega.
#[test]
fn valid_control_without_control_write_is_not_queued_for_the_application() {
    // Caso denegado: `control_read` (servidor→cliente) pero no `control_write`.
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon-res-control", true);
    let client_leaf = ca.leaf("openrelay-client-res-control", false);
    let (mut client, mut server) = session_pair(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_control_read(),
    );
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24)
        .expect("handshake mTLS");
    let session = authorized_pair(&mut client, &mut server).expect("sesión autorizada");
    assert_eq!(
        server.capabilities(session),
        Some(Capabilities::none().with_control_read()),
        "las capacidades del caso denegado son las declaradas, no un fallo de ACL"
    );

    let denied = ping("res-control-denegado");
    client.send_control(&denied).expect("cliente envía control");
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 16)
        .expect("control en vuelo");
    assert!(
        !client.conn.is_handshaking() && !client.conn.is_closed(),
        "no se cerró la sesión por el control denegado"
    );
    assert!(
        queued_control(&server, session) == Some(0),
        "residuo: control válido sin `control_write` no debe quedar en la cola de aplicación"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "la lectura pública sigue denegando"
    );
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "la identidad no se revoca por el control denegado"
    );

    // Control positivo del mismo tráfico: `control_write` concedido sí puebla.
    // Va en su propio runner para observarlo aunque el caso denegado sea RED.
}

/// Control positivo del caso de control: con `control_write` concedido, el
/// **mismo** mensaje que el caso denegado sí puebla la cola y `recv_control` lo
/// entrega.
#[test]
fn control_with_control_write_is_queued_for_the_application() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon-res-control-pos", true);
    let client_leaf = ca.leaf("openrelay-client-res-control-pos", false);
    let (mut client, mut server) = session_pair(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_control_write(),
    );
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24)
        .expect("handshake mTLS");
    let session = authorized_pair(&mut client, &mut server).expect("sesión autorizada");
    let granted = ping("res-control-concedido");
    client
        .send_control(&granted)
        .expect("cliente envía control");
    session_exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 16)
        .expect("control en vuelo");
    assert!(
        queued_control(&server, session) == Some(1),
        "control positivo: con `control_write` el mismo tráfico sí queda encolado una vez"
    );
    assert_eq!(
        server.recv_control(session),
        Some(granted),
        "control positivo: la lectura pública entrega el mensaje encolado"
    );
}
