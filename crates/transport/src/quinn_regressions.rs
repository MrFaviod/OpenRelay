//! Regresiones A sobre `Connection` QUIC real: prefijo de control inválido.
//!
//! Estos casos no usan `SessionClient::send_control` (que codifica con
//! `wire::encode_message` y nunca puede emitir un prefijo inválido): tras el
//! handshake mTLS+ACL, el peer escribe bytes crudos en un stream bidireccional
//! de su `Connection` real. El receptor debe cerrar con `0x104`, no entregar
//! nada residual y el peer debe perder la autorización al bombear el cierre.
//!
//! Módulo hijo de `quinn`: accede a los campos privados de `SessionClient` y a
//! la `Connection` real sin añadir superficie pública. Las fixtures (CA, hojas,
//! ACL) son test-only y viven sólo en memoria.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use openrelay_proto::wire::ControlMessage;
use quinn_proto::crypto::rustls::QuicClientConfig;
use quinn_proto::{
    ClientConfig as QuinnClientConfig, Connection, ConnectionError, ConnectionHandle,
    DatagramEvent, Dir, Endpoint, EndpointConfig, Event, TransportConfig,
};
use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig as TlsClientConfig, RootCertStore};

use super::{
    Acl, Capabilities, ClientEvent, ServerEvent, SessionClient, SessionLimits, SessionServer,
    TransportError,
};
use crate::identity::{self, PinnedServerVerifier, SpkiPin, StaticClientCertResolver};
use crate::ALPN_SESSION;

const SERVER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44331);
const CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 44332);
/// Código de cierre por framing de control corrupto (contrato del transporte).
const CLOSE_CONTROL_FRAMING: u32 = 0x104;

// ---------------------------------------------------------------------------
// Fixtures test-only: CA local, hojas ECDSA P-256, claves sólo en memoria.
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
            .push(DnType::CommonName, "openrelay-test-ca-t10a");
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

fn acl_for(leaf: &TestLeaf) -> Acl {
    let mut acl = Acl::new();
    acl.allow(
        leaf.pin,
        Capabilities::none()
            .with_mic()
            .with_control_read()
            .with_control_write(),
    );
    acl
}

// ---------------------------------------------------------------------------
// Enlace en memoria sobre datagramas QUIC reales.
// ---------------------------------------------------------------------------

trait Wire {
    fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)>;
    fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), String>;
}

impl Wire for SessionServer {
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

impl Wire for SessionClient {
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

/// Peer QUIC crudo: mismo TLS/mTLS/ALPN/pin que el cliente real, pero el
/// llamador ve directamente los `Event` de quinn (incluido el código de cierre
/// de aplicación del peer), que la superficie de sesión no reexpone.
struct RawPeer {
    endpoint: Endpoint,
    conn: Connection,
    handle: ConnectionHandle,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
    scratch: Vec<u8>,
    /// Cierre de aplicación observado en el wire, si llegó.
    application_close: Option<u32>,
}

impl RawPeer {
    fn connect(client_leaf: &TestLeaf, server_leaf: &TestLeaf, ca: &TestCa, now: Instant) -> Self {
        let verifier =
            PinnedServerVerifier::new(roots(ca), server_leaf.pin).expect("verificador pinneado");
        let mut tls = TlsClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_client_cert_resolver(Arc::new(StaticClientCertResolver::new(
                client_leaf.certified(ca),
            )));
        tls.alpn_protocols = vec![ALPN_SESSION.to_vec()];
        tls.enable_early_data = false;
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
            application_close: None,
        }
    }

    fn is_authorized(&self) -> bool {
        !self.conn.is_handshaking() && !self.conn.is_closed()
    }
}

impl Wire for RawPeer {
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
    ) -> Result<(), String> {
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
}

impl RawPeer {
    fn observe(&mut self) {
        while let Some(event) = self.conn.poll_endpoint_events() {
            if let Some(event) = self.endpoint.handle_event(self.handle, event) {
                self.conn.handle_event(event);
            }
        }
        while let Some(event) = self.conn.poll() {
            if let Event::ConnectionLost {
                reason: ConnectionError::ApplicationClosed(close),
            } = event
            {
                self.application_close = Some(close.error_code.into_inner() as u32);
            }
        }
    }
}

fn drain<A: Wire, B: Wire>(
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

fn exchange<A: Wire, B: Wire>(client: &mut A, server: &mut B, rounds: usize) -> Result<(), String> {
    let now = Instant::now();
    for _ in 0..rounds {
        drain(client, CLIENT_ADDR, server, now)?;
        drain(server, SERVER_ADDR, client, now)?;
    }
    Ok(())
}

/// Servidor real con ACL explícita para la hoja de cliente indicada.
fn server_for(server_leaf: &TestLeaf, client_leaf: &TestLeaf, ca: &TestCa) -> SessionServer {
    SessionServer::new(
        server_leaf.certified(ca),
        roots(ca),
        acl_for(client_leaf),
        SessionLimits::default(),
    )
    .expect("servidor de sesión")
}

fn authorized_session(server: &mut SessionServer) -> Option<super::SessionId> {
    while let Some(event) = server.next_event() {
        if let ServerEvent::Authorized { session, .. } = event {
            return Some(session);
        }
    }
    None
}

/// Escribe `prefix` crudo en un stream bidireccional nuevo de `conn`.
fn inject_raw_prefix(conn: &mut Connection, prefix: &[u8]) {
    let stream = conn.streams().open(Dir::Bi).expect("stream bidireccional");
    let mut written = 0usize;
    while written < prefix.len() {
        match conn.send_stream(stream).write(&prefix[written..]) {
            Ok(0) => panic!("escritura bloqueada sin progreso"),
            Ok(count) => written += count,
            Err(error) => panic!("escritura cruda rechazada: {error:?}"),
        }
    }
    assert_eq!(written, prefix.len(), "prefijo crudo completo en el stream");
}

// ---------------------------------------------------------------------------
// Casos.
// ---------------------------------------------------------------------------

/// Caso A1: prefijo `u32BE` = 0 (longitud cero) escrito crudo tras el handshake
/// mTLS+ACL. El receptor cierra `0x104`, no entrega control/audio/identidad ni
/// capacidades residuales, y el peer pierde la autorización al bombear el cierre.
#[test]
fn quinn_regressions_zero_prefix_closes_0x104_and_drops_residuals() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10a", true);
    let client_leaf = ca.leaf("openrelay-client-t10a", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut client = SessionClient::connect(
        client_leaf.certified(&ca),
        roots(&ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");

    exchange(&mut client, &mut server, 24).expect("handshake mTLS");
    assert!(
        client.is_authorized(),
        "cliente autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "identidad de peer presente antes de la violación"
    );

    inject_raw_prefix(&mut client.conn, &0u32.to_be_bytes());
    exchange(&mut client, &mut server, 32).expect("bombeo del prefijo inválido");

    assert_eq!(
        server.recv_control(session),
        None,
        "ningún mensaje de control entregado tras prefijo 0"
    );
    assert_eq!(
        server.recv_audio(session),
        None,
        "ningún frame de audio entregado tras prefijo 0"
    );
    assert_eq!(
        server.identity(session),
        None,
        "sin identidad residual: la sesión no sigue autorizada"
    );
    assert_eq!(
        server.capabilities(session),
        None,
        "sin capacidades residuales tras el cierre"
    );

    assert!(
        !client.is_authorized(),
        "el peer pierde la autorización al bombear el cierre"
    );
    assert!(
        matches!(
            client.send_control(&ControlMessage::Ping {
                request_id: "post-close".to_string(),
            }),
            Err(TransportError::Unauthorized)
        ),
        "el peer cerrado ya no puede escribir control"
    );
    let closed = std::iter::from_fn(|| client.next_event())
        .any(|event| matches!(event, ClientEvent::Closed));
    assert!(closed, "el peer observa ClientEvent::Closed");
}

/// Caso A2: prefijo `u32BE` = 16385 (un byte por encima del máximo 16384).
/// Se usa el peer crudo para registrar el código de cierre de aplicación
/// observado: `Some(0x104)` si el receptor rechaza, `None` si no cierra.
#[test]
fn quinn_regressions_over_limit_prefix_closes_0x104_and_drops_residuals() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10a-2", true);
    let client_leaf = ca.leaf("openrelay-client-t10a-2", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(peer.is_authorized(), "peer crudo con handshake completo");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    inject_raw_prefix(&mut peer.conn, &16_385u32.to_be_bytes());
    exchange(&mut peer, &mut server, 32).expect("bombeo del prefijo inválido");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "el peer debe recibir ApplicationClosed(0x104) tras prefijo 16385"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "ningún mensaje de control entregado tras prefijo 16385"
    );
    assert_eq!(
        server.recv_audio(session),
        None,
        "ningún frame de audio entregado tras prefijo 16385"
    );
    assert_eq!(
        server.identity(session),
        None,
        "sin identidad residual: la sesión no sigue autorizada"
    );
    assert_eq!(
        server.capabilities(session),
        None,
        "sin capacidades residuales tras el cierre"
    );
}

/// Caso A3: el código de cierre de aplicación que llega al peer es `0x104`.
/// Se observa con un peer QUIC crudo (mismo TLS/mTLS/ALPN/pin) porque la
/// superficie de sesión no reexpone el código, sólo `ClientEvent::Closed`.
#[test]
fn quinn_regressions_peer_observes_application_close_0x104() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10a-3", true);
    let client_leaf = ca.leaf("openrelay-client-t10a-3", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(peer.is_authorized(), "peer crudo con handshake completo");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    inject_raw_prefix(&mut peer.conn, &0u32.to_be_bytes());
    exchange(&mut peer, &mut server, 32).expect("bombeo del prefijo inválido");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "el peer recibe ApplicationClosed(0x104) del receptor"
    );
    assert_eq!(
        server.identity(session),
        None,
        "el receptor no conserva identidad autorizada"
    );
}

/// Control experimental: un frame de control válido sigue entregándose. Si este
/// caso falla, el rojo de los anteriores no es atribuible al framing.
#[test]
fn quinn_regressions_valid_control_frame_is_still_delivered() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10a-4", true);
    let client_leaf = ca.leaf("openrelay-client-t10a-4", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut client = SessionClient::connect(
        client_leaf.certified(&ca),
        roots(&ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");

    exchange(&mut client, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let ping = ControlMessage::Ping {
        request_id: "t10a-control".to_string(),
    };
    client.send_control(&ping).expect("control válido");
    exchange(&mut client, &mut server, 8).expect("entrega de control");

    assert_eq!(
        server.recv_control(session),
        Some(ping),
        "un frame bien enmarcado se entrega con la misma sesión autorizada"
    );
    assert!(client.is_authorized(), "la sesión sigue viva sin violación");
}

// Regresiones B/D: módulos test-only con ownership independiente.
#[path = "quinn_regressions_b.rs"]
mod b;
#[path = "quinn_regressions_d.rs"]
mod d;
