//! Frontera de enrolamiento: ALPN `openrelay-enroll/1`, servidor efímero sin
//! certificado de cliente, sin DATAGRAM, sin tickets y sin 0-RTT.
//!
//! El cliente valida el pin SPKI del certificado efímero **antes** de enviar el
//! token. El servidor compara el token en tiempo constante, exige ventana
//! vigente y de un solo uso, y cierra cualquier conexión que intente audio,
//! control de sesión o tickets. El enrolamiento no crea identidad persistente
//! ni ACL: eso queda fuera de este módulo.
//!
//! Los mensajes son JSON acotado con prefijo `u32` big-endian, igual que el
//! framing de control, pero con un contrato propio y mínimo:
//!
//! ```json
//! {"type":"enroll.request","version":1,"token":"<64 hex>","spki":"<hex DER>"}
//! {"type":"enroll.result","version":1,"status":"accepted",
//!  "client_leaf":"<hex DER>","session_ca":"<hex DER>","session_port":47000}
//! {"type":"enroll.result","version":1,"status":"rejected","reason":"expired"}
//! ```
//!
//! El resultado `accepted` no se escribe cuando el token es válido: el
//! servidor espera la decisión local (`approve`/`reject_pending`, o la
//! expiración de la ventana) porque la CA que firma la hoja de sesión vive en
//! el daemon, no en este crate.

use std::collections::VecDeque;
use std::fmt;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use quinn_proto::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use quinn_proto::{
    ClientConfig as QuinnClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint,
    EndpointConfig, Event, Incoming, ReadError, ServerConfig as QuinnServerConfig, StreamEvent,
    StreamId, TransportConfig, VarInt,
};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig as TlsClientConfig, RootCertStore, ServerConfig as TlsServerConfig};
use serde_json::{json, Value};

use x509_cert::der::Decode;
use x509_cert::spki::SubjectPublicKeyInfoOwned;

use crate::identity::{IdentityError, PinnedServerVerifier, SpkiPin, StaticServerCertResolver};
use crate::ALPN_ENROLL;

/// Bytes de un token de enrolamiento.
pub const ENROLL_TOKEN_LEN: usize = 32;
/// TTL máximo de una ventana de enrolamiento.
pub const MAX_ENROLL_TTL: Duration = Duration::from_secs(300);
/// Payload JSON máximo de un mensaje de enrolamiento. El resultado aceptado
/// lleva dos certificados DER en hexadecimal (hoja de sesión y CA de sesión).
pub const MAX_ENROLL_PAYLOAD: usize = 8 * 1024;
/// Versión del contrato de enrolamiento.
pub const ENROLL_VERSION: u64 = 1;

/// Resultado de un enrolamiento aprobado.
///
/// `client_leaf_der` es la hoja `clientAuth` firmada por la CA local cuyo SPKI
/// coincide con el que declaró el cliente. `session_ca_der` es la CA que el
/// cliente debe poner en el `RootCertStore` del servidor de **sesión**.
/// `session_port` es el puerto UDP donde escucha ese servidor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollOutcome {
    /// Certificado DER de la hoja de cliente.
    pub client_leaf_der: Vec<u8>,
    /// CA DER que firma el certificado del servidor de sesión.
    pub session_ca_der: Vec<u8>,
    /// Puerto UDP del servidor de sesión.
    pub session_port: u16,
}

/// Token de un solo uso. Su `Debug` no lo imprime.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollmentToken([u8; ENROLL_TOKEN_LEN]);

impl EnrollmentToken {
    /// Construye el token desde bytes ya generados por la terminal.
    pub fn from_bytes(bytes: [u8; ENROLL_TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    /// Genera un token con el RNG del sistema.
    pub fn generate() -> Result<Self, EnrollError> {
        let rng = ring::rand::SystemRandom::new();
        let mut bytes = [0u8; ENROLL_TOKEN_LEN];
        ring::rand::SecureRandom::fill(&rng, &mut bytes)
            .map_err(|_| EnrollError::Config("system RNG not available"))?;
        Ok(Self(bytes))
    }

    /// Bytes para construir el QR. No registrar ni incluir en logs.
    pub fn as_bytes(&self) -> &[u8; ENROLL_TOKEN_LEN] {
        &self.0
    }

    /// Hexadecimal para el QR. No registrar.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(ENROLL_TOKEN_LEN * 2);
        for byte in self.0 {
            fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}")).expect("String no falla");
        }
        out
    }
}

impl fmt::Debug for EnrollmentToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EnrollmentToken(<redacted>)")
    }
}

/// Motivo de rechazo de enrolamiento.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnrollReject {
    /// No hay ventana abierta.
    NoWindow,
    /// La ventana expiró.
    Expired,
    /// El token ya se usó.
    Reused,
    /// El token no coincide.
    Mismatch,
    /// Versión desconocida.
    Version,
    /// Mensaje malformado.
    Malformed,
    /// El usuario local no aprobó el dispositivo.
    Denied,
}

impl EnrollReject {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoWindow => "no_window",
            Self::Expired => "expired",
            Self::Reused => "reused",
            Self::Mismatch => "mismatch",
            Self::Version => "version",
            Self::Malformed => "malformed",
            Self::Denied => "denied",
        }
    }
}

impl fmt::Display for EnrollReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Evento de la frontera de enrolamiento.
#[derive(Debug)]
pub enum EnrollmentEvent {
    /// Handshake de enrolamiento completo.
    Ready,
    /// Token aceptado. Lleva el pin SPKI de la identidad que el cliente
    /// declaró usar; todavía no es una identidad aprobada.
    Accepted(SpkiPin),
    /// Token rechazado con motivo.
    Rejected(EnrollReject),
    /// Mensaje de una superficie prohibida (audio/control/ticket).
    ForbiddenSurface(&'static str),
    /// Conexión cerrada.
    Closed,
}

/// Error de enrolamiento.
#[derive(Debug)]
#[non_exhaustive]
pub enum EnrollError {
    /// Configuración inválida.
    Config(&'static str),
    /// Identidad inválida.
    Identity(IdentityError),
    /// TLS inválido.
    Tls(String),
    /// Conexión fallida.
    Connect(String),
    /// La conexión no está lista.
    NotReady,
    /// La conexión está cerrada.
    Closed,
    /// Mensaje excede el límite antes de parsear.
    Oversized,
    /// JSON inválido o forma inesperada.
    Malformed,
    /// Ventana ya abierta.
    WindowOpen,
    /// El servidor rechazó el enrolamiento (motivo del resultado).
    Rejected(EnrollReject),
    /// La espera bloqueante agotó su plazo.
    Timeout,
    /// Stream rechazado.
    Stream(&'static str),
}

impl fmt::Display for EnrollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(reason) => write!(f, "invalid configuration: {reason}"),
            Self::Identity(error) => write!(f, "invalid identity: {error}"),
            Self::Tls(detail) => write!(f, "invalid TLS: {detail}"),
            Self::Connect(detail) => write!(f, "connection failed: {detail}"),
            Self::NotReady => write!(f, "enrollment not ready"),
            Self::Closed => write!(f, "enrollment closed"),
            Self::Oversized => write!(f, "enrollment message too large"),
            Self::Malformed => write!(f, "malformed enrollment message"),
            Self::WindowOpen => write!(f, "an enrollment window is already open"),
            Self::Rejected(reason) => write!(f, "enrollment rejected: {reason}"),
            Self::Timeout => write!(f, "enrollment timed out"),
            Self::Stream(reason) => write!(f, "stream rejected: {reason}"),
        }
    }
}

impl std::error::Error for EnrollError {}

impl From<IdentityError> for EnrollError {
    fn from(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}

fn enroll_transport() -> Arc<TransportConfig> {
    let mut config = TransportConfig::default();
    // La aprobación es humana: la conexión queda ociosa mientras el PC espera
    // el `y`. Un idle timeout corto la cerraba antes de entregar la hoja.
    config.max_idle_timeout(Some(
        quinn_proto::IdleTimeout::try_from(MAX_ENROLL_TTL + Duration::from_secs(60))
            .expect("valid timeout"),
    ));
    config.max_concurrent_bidi_streams(1u32.into());
    config.max_concurrent_uni_streams(0u32.into());
    config.stream_receive_window(16_384u32.into());
    config.receive_window(32_768u32.into());
    config.datagram_receive_buffer_size(None);
    config.datagram_send_buffer_size(0);
    Arc::new(config)
}

fn encode_frame(value: &Value) -> Result<Vec<u8>, EnrollError> {
    let payload = serde_json::to_vec(value).map_err(|_| EnrollError::Malformed)?;
    if payload.is_empty() || payload.len() > MAX_ENROLL_PAYLOAD {
        return Err(EnrollError::Oversized);
    }
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn parse_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

fn parse_version(value: &Value) -> Option<u64> {
    value.get("version").and_then(Value::as_u64)
}

fn decode_hex32(text: &str) -> Option<[u8; ENROLL_TOKEN_LEN]> {
    if text.len() != ENROLL_TOKEN_LEN * 2 {
        return None;
    }
    let mut out = [0u8; ENROLL_TOKEN_LEN];
    let bytes = text.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let high = (bytes[index * 2] as char).to_digit(16)?;
        let low = (bytes[index * 2 + 1] as char).to_digit(16)?;
        *slot = ((high << 4) | low) as u8;
    }
    Some(out)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}")).expect("String no falla");
    }
    out
}

/// Pin del `SubjectPublicKeyInfo` DER enviado en hex. `None` si no es hex
/// válido o si el DER no parsea como SPKI.
fn decode_spki_pin(text: &str) -> Option<SpkiPin> {
    if text.is_empty() || text.len() % 2 != 0 {
        return None;
    }
    let bytes = text
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect::<Option<Vec<u8>>>()?;
    SubjectPublicKeyInfoOwned::from_der(&bytes).ok()?;
    Some(SpkiPin::from_spki_der(&bytes))
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || text.len() % 2 != 0 {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

fn ct_eq(left: &[u8; ENROLL_TOKEN_LEN], right: &[u8; ENROLL_TOKEN_LEN]) -> bool {
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

struct Window {
    token: EnrollmentToken,
    expires: Instant,
    used: bool,
}

/// Solicitud válida esperando la decisión local. Guarda el SPKI en claro: el
/// daemon lo necesita para firmar la hoja de sesión.
struct Decision {
    pin: SpkiPin,
    spki: Vec<u8>,
    expires: Instant,
}

/// Servidor de enrolamiento sans-IO de una sola ventana.
pub struct EnrollServer {
    endpoint: Endpoint,
    accept_config: Arc<QuinnServerConfig>,
    window: Option<Window>,
    /// Token ya consumido, esperando aprobación/rechazo locales.
    decision: Option<Decision>,
    pending: VecDeque<Incoming>,
    conn: Option<Connection>,
    handle: Option<ConnectionHandle>,
    ready: bool,
    closed: bool,
    events: VecDeque<EnrollmentEvent>,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
    recv_stream: Option<StreamId>,
    send_stream: Option<StreamId>,
    rx: Vec<u8>,
    scratch: Vec<u8>,
    attempts: u32,
}

impl EnrollServer {
    /// Construye el servidor con el certificado efímero de la ventana.
    ///
    /// No pide certificado de cliente: la ausencia de identidad es explícita y
    /// sólo habilita mensajes de enrolamiento.
    pub fn new(identity: Arc<CertifiedKey>) -> Result<Self, EnrollError> {
        let mut tls = TlsServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(StaticServerCertResolver::new(identity)));
        tls.alpn_protocols = vec![ALPN_ENROLL.to_vec()];
        tls.send_tls13_tickets = 0;
        tls.max_early_data_size = 0;
        let crypto =
            QuicServerConfig::try_from(tls).map_err(|error| EnrollError::Tls(error.to_string()))?;
        let mut quinn = QuinnServerConfig::with_crypto(Arc::new(crypto));
        quinn.transport_config(enroll_transport());
        let accept_config = Arc::new(quinn);
        let endpoint = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            Some(accept_config.clone()),
            false,
            None,
        );
        Ok(Self {
            endpoint,
            accept_config,
            window: None,
            decision: None,
            pending: VecDeque::new(),
            conn: None,
            handle: None,
            ready: false,
            closed: false,
            events: VecDeque::new(),
            outgoing: VecDeque::new(),
            recv_stream: None,
            send_stream: None,
            rx: Vec::new(),
            scratch: Vec::new(),
            attempts: 0,
        })
    }

    /// Abre la única ventana de enrolamiento.
    pub fn open_window(
        &mut self,
        token: EnrollmentToken,
        ttl: Duration,
        now: Instant,
    ) -> Result<(), EnrollError> {
        if self.window.is_some() {
            return Err(EnrollError::WindowOpen);
        }
        if ttl.is_zero() || ttl > MAX_ENROLL_TTL {
            return Err(EnrollError::Config("enrollment ttl out of 1..=300 s"));
        }
        self.window = Some(Window {
            token,
            expires: now + ttl,
            used: false,
        });
        Ok(())
    }

    /// Procesa un datagrama del cliente.
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), EnrollError> {
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
            Some(DatagramEvent::NewConnection(incoming)) => {
                if self.pending.len() < 2 {
                    self.pending.push_back(incoming);
                }
            }
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if self.handle == Some(handle) {
                    if let Some(conn) = self.conn.as_mut() {
                        conn.handle_event(event);
                    }
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                self.outgoing
                    .push_back((transmit.destination, response[..transmit.size].to_vec()));
            }
            None => {}
        }
        self.pump(now)
    }

    /// Siguiente evento.
    pub fn next_event(&mut self) -> Option<EnrollmentEvent> {
        self.events.pop_front()
    }

    /// Siguiente datagrama a enviar.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        if let Some(item) = self.outgoing.pop_front() {
            return Some(item);
        }
        let conn = self.conn.as_mut()?;
        self.scratch.clear();
        let transmit = conn.poll_transmit(now, 1, &mut self.scratch)?;
        Some((transmit.destination, self.scratch[..transmit.size].to_vec()))
    }

    /// ¿Handshake completo?
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Intentos de `enroll.request` recibidos (válidos o no). Un pin de
    /// servidor incorrecto debe dejar este contador en cero: el cliente aborta
    /// el handshake antes de enviar el token.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Cierra la ventana y destruye el estado temporal.
    pub fn close_window(&mut self) {
        self.window = None;
        self.decision = None;
    }

    /// Pin de la solicitud válida que espera decisión local.
    pub fn pending(&self) -> Option<SpkiPin> {
        self.decision.as_ref().map(|decision| decision.pin)
    }

    /// SPKI DER declarado por la solicitud pendiente (el que firma la CA).
    pub fn pending_spki(&self) -> Option<&[u8]> {
        self.decision
            .as_ref()
            .map(|decision| decision.spki.as_slice())
    }

    /// Aprueba la solicitud pendiente y escribe el resultado en el stream.
    ///
    /// La hoja y la CA las aporta el dueño de la CA (el daemon). Este crate no
    /// las valida: sólo las transporta. Una solicitud expirada se rechaza.
    pub fn approve(&mut self, now: Instant, outcome: &EnrollOutcome) -> Result<(), EnrollError> {
        let Some(decision) = self.decision.take() else {
            return Err(EnrollError::NotReady);
        };
        let Some(mut conn) = self.conn.take() else {
            return Err(EnrollError::Closed);
        };
        if now >= decision.expires {
            self.reject(&mut conn, now, EnrollReject::Expired);
            self.conn = Some(conn);
            return Err(EnrollError::Rejected(EnrollReject::Expired));
        }
        self.respond(
            &mut conn,
            json!({
                "type":"enroll.result",
                "version":ENROLL_VERSION,
                "status":"accepted",
                "client_leaf": to_hex(&outcome.client_leaf_der),
                "session_ca": to_hex(&outcome.session_ca_der),
                "session_port": outcome.session_port,
            }),
        );
        self.conn = Some(conn);
        Ok(())
    }

    /// Rechaza la solicitud pendiente con motivo y cierra la conexión.
    pub fn reject_pending(
        &mut self,
        now: Instant,
        reason: EnrollReject,
    ) -> Result<(), EnrollError> {
        let Some(_decision) = self.decision.take() else {
            return Err(EnrollError::NotReady);
        };
        let Some(mut conn) = self.conn.take() else {
            return Err(EnrollError::Closed);
        };
        self.reject(&mut conn, now, reason);
        self.conn = Some(conn);
        Ok(())
    }

    /// Expira la solicitud pendiente (la ventana TTL es el plazo de decisión).
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), EnrollError> {
        if self
            .decision
            .as_ref()
            .is_some_and(|decision| now >= decision.expires)
        {
            let _ = self.reject_pending(now, EnrollReject::Expired);
        }
        Ok(())
    }

    fn pump(&mut self, now: Instant) -> Result<(), EnrollError> {
        if self.conn.is_none() {
            if let Some(incoming) = self.pending.pop_front() {
                let mut response = Vec::new();
                match self.endpoint.accept(
                    incoming,
                    now,
                    &mut response,
                    Some(Arc::clone(&self.accept_config)),
                ) {
                    Ok((handle, conn)) => {
                        self.handle = Some(handle);
                        self.conn = Some(conn);
                    }
                    Err(error) => {
                        if let Some(transmit) = error.response {
                            self.outgoing.push_back((
                                transmit.destination,
                                response[..transmit.size].to_vec(),
                            ));
                        }
                    }
                }
            }
        }

        let Some(mut conn) = self.conn.take() else {
            return Ok(());
        };
        let handle = self.handle.expect("connection with handle");

        while let Some(event) = conn.poll_endpoint_events() {
            if let Some(event) = self.endpoint.handle_event(handle, event) {
                conn.handle_event(event);
            }
        }

        while let Some(event) = conn.poll() {
            match event {
                Event::Connected => {
                    let handshake = conn
                        .crypto_session()
                        .handshake_data()
                        .and_then(|data| data.downcast::<HandshakeData>().ok());
                    if handshake.and_then(|data| data.protocol).as_deref() != Some(ALPN_ENROLL) {
                        conn.close(now, VarInt::from_u32(0x200), Bytes::from_static(b"alpn"));
                        self.closed = true;
                        self.events
                            .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed));
                        self.conn = Some(conn);
                        return Ok(());
                    }
                    self.ready = true;
                    self.events.push_back(EnrollmentEvent::Ready);
                }
                Event::ConnectionLost { .. } => {
                    if !self.closed {
                        self.closed = true;
                        self.events.push_back(EnrollmentEvent::Closed);
                    }
                    return Ok(());
                }
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                    if !self.ready {
                        conn.close(now, VarInt::from_u32(0x201), Bytes::from_static(b"early"));
                        self.closed = true;
                        self.conn = Some(conn);
                        return Ok(());
                    }
                    while let Some(stream) = conn.streams().accept(Dir::Bi) {
                        if self.recv_stream.is_none() {
                            self.recv_stream = Some(stream);
                        } else {
                            conn.close(
                                now,
                                VarInt::from_u32(0x202),
                                Bytes::from_static(b"extra stream"),
                            );
                            self.closed = true;
                            self.conn = Some(conn);
                            return Ok(());
                        }
                        // Los bytes pueden estar ya disponibles al aceptar; Quinn
                        // no reemite `Readable` para ellos.
                        self.read_messages(&mut conn, now);
                        if self.closed {
                            self.conn = Some(conn);
                            return Ok(());
                        }
                    }
                }
                Event::Stream(StreamEvent::Readable { id }) => {
                    if self.recv_stream != Some(id) {
                        continue;
                    }
                    self.read_messages(&mut conn, now);
                    if self.closed {
                        self.conn = Some(conn);
                        return Ok(());
                    }
                }
                _ => {}
            }
        }

        self.conn = Some(conn);
        Ok(())
    }

    fn read_messages(&mut self, conn: &mut Connection, now: Instant) {
        loop {
            let Some(stream) = self.recv_stream else {
                return;
            };
            let (data, overflow, fin) = {
                let mut recv = conn.recv_stream(stream);
                let Ok(mut chunks) = recv.read(true) else {
                    return;
                };
                let mut data = Vec::new();
                let mut overflow = false;
                let mut fin = false;
                loop {
                    match chunks.next(usize::MAX) {
                        Ok(Some(chunk)) => {
                            if self.rx.len() + data.len() + chunk.bytes.len()
                                > MAX_ENROLL_PAYLOAD + 4
                            {
                                overflow = true;
                                break;
                            }
                            data.extend_from_slice(&chunk.bytes);
                        }
                        // `Ok(None)` no es `Blocked`: quinn-proto sólo lo devuelve
                        // cuando el FIN contiguo ya se consumió (recv.rs). `Blocked`
                        // es un hueco aún no recibido y debe seguir esperando.
                        Ok(None) => {
                            fin = true;
                            break;
                        }
                        // `Blocked`: sin más datos por ahora; se conserva el prefijo.
                        Err(ReadError::Blocked) => break,
                        Err(ReadError::Reset(_)) => {
                            overflow = true;
                            break;
                        }
                    }
                }
                let _ = chunks.finalize();
                (data, overflow, fin)
            };
            if overflow {
                self.fail(conn, now, EnrollReject::Malformed, 0x203);
                return;
            }
            self.rx.extend_from_slice(&data);
            while self.rx.len() >= 4 {
                let len =
                    u32::from_be_bytes([self.rx[0], self.rx[1], self.rx[2], self.rx[3]]) as usize;
                if len == 0 || len > MAX_ENROLL_PAYLOAD {
                    self.fail(conn, now, EnrollReject::Malformed, 0x205);
                    return;
                }
                if self.rx.len() < 4 + len {
                    break;
                }
                let payload = self.rx[4..4 + len].to_vec();
                self.rx.drain(..4 + len);
                self.handle_message(conn, now, &payload);
                if self.closed {
                    return;
                }
            }
            // EOF real con bytes sin consumir: el peer declaró FIN sobre un
            // frame incompleto. Fail-closed y sin conservar el prefijo parcial.
            if fin && !self.rx.is_empty() {
                self.rx.clear();
                self.fail(conn, now, EnrollReject::Malformed, 0x204);
                return;
            }
            if data.is_empty() {
                return;
            }
        }
    }

    fn handle_message(&mut self, conn: &mut Connection, now: Instant, payload: &[u8]) {
        let value: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(_) => {
                self.fail(conn, now, EnrollReject::Malformed, 0x206);
                return;
            }
        };
        let Some(kind) = parse_type(&value) else {
            self.fail(conn, now, EnrollReject::Malformed, 0x207);
            return;
        };
        match kind {
            "audio" | "control" | "ticket" | "session" => {
                let surface = match kind {
                    "audio" => "audio",
                    "control" => "control",
                    "ticket" => "ticket",
                    _ => "session",
                };
                self.events
                    .push_back(EnrollmentEvent::ForbiddenSurface(surface));
                conn.close(
                    now,
                    VarInt::from_u32(0x210),
                    Bytes::from_static(b"forbidden surface"),
                );
                self.closed = true;
            }
            "enroll.request" => {
                self.attempts = self.attempts.saturating_add(1);
                if parse_version(&value) != Some(ENROLL_VERSION) {
                    self.reject(conn, now, EnrollReject::Version);
                    return;
                }
                let Some(token_text) = value.get("token").and_then(Value::as_str) else {
                    self.reject(conn, now, EnrollReject::Malformed);
                    return;
                };
                let Some(token) = decode_hex32(token_text) else {
                    self.reject(conn, now, EnrollReject::Malformed);
                    return;
                };
                let Some(spki_text) = value.get("spki").and_then(Value::as_str) else {
                    self.reject(conn, now, EnrollReject::Malformed);
                    return;
                };
                let Some(identity) = decode_spki_pin(spki_text) else {
                    self.reject(conn, now, EnrollReject::Malformed);
                    return;
                };
                let now_reject = match self.window.as_ref() {
                    None => Some(EnrollReject::NoWindow),
                    Some(window) if window.used => Some(EnrollReject::Reused),
                    Some(window) if now >= window.expires => Some(EnrollReject::Expired),
                    Some(_) => None,
                };
                if let Some(reason) = now_reject {
                    self.reject(conn, now, reason);
                    return;
                }
                let window = self.window.as_mut().expect("ventana vigente");
                if !ct_eq(window.token.as_bytes(), &token) {
                    self.reject(conn, now, EnrollReject::Mismatch);
                    return;
                }
                window.used = true;
                // El token es válido, pero la respuesta espera la decisión
                // local: el daemon firma la hoja y llama a `approve`.
                let expires = window.expires;
                self.decision = Some(Decision {
                    pin: identity,
                    spki: decode_hex(spki_text).expect("SPKI ya validado"),
                    expires,
                });
                self.events.push_back(EnrollmentEvent::Accepted(identity));
                self.window = None;
            }
            _ => self.fail(conn, now, EnrollReject::Malformed, 0x208),
        }
    }

    fn reject(&mut self, conn: &mut Connection, now: Instant, reason: EnrollReject) {
        self.respond(
            conn,
            json!({
                "type":"enroll.result",
                "version":ENROLL_VERSION,
                "status":"rejected",
                "reason": reason.as_str()
            }),
        );
        self.events.push_back(EnrollmentEvent::Rejected(reason));
        conn.close(
            now,
            VarInt::from_u32(0x211),
            Bytes::from_static(b"enroll rejected"),
        );
        self.closed = true;
    }

    fn fail(&mut self, conn: &mut Connection, now: Instant, reason: EnrollReject, code: u32) {
        self.events.push_back(EnrollmentEvent::Rejected(reason));
        conn.close(
            now,
            VarInt::from_u32(code),
            Bytes::from_static(b"enroll malformed"),
        );
        self.closed = true;
    }

    fn respond(&mut self, conn: &mut Connection, value: Value) {
        let Ok(frame) = encode_frame(&value) else {
            return;
        };
        let stream = match self.send_stream {
            Some(stream) => stream,
            None => {
                let Some(stream) = self.recv_stream else {
                    return;
                };
                self.send_stream = Some(stream);
                stream
            }
        };
        let mut written = 0usize;
        while written < frame.len() {
            match conn.send_stream(stream).write(&frame[written..]) {
                Ok(0) | Err(_) => return,
                Ok(count) => written += count,
            }
        }
        let _ = conn.send_stream(stream).finish();
    }
}

/// Cliente de enrolamiento sans-IO.
pub struct EnrollClient {
    endpoint: Endpoint,
    conn: Connection,
    handle: ConnectionHandle,
    ready: bool,
    events: VecDeque<EnrollmentEvent>,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
    send_stream: Option<StreamId>,
    rx: Vec<u8>,
    scratch: Vec<u8>,
    /// Cierre fail-closed consumado: ni `Ready` ni `Accepted` pueden reaparecer.
    terminal: bool,
    /// Pin enviado en el último `enroll.request`; el `Accepted` lo devuelve.
    sent_identity: Option<SpkiPin>,
    /// Resultado aceptado ya parseado (hoja, CA y puerto de sesión).
    outcome: Option<EnrollOutcome>,
    /// Socket real para el modo bloqueante de [`Self::send_token`]. `None` en
    /// el cliente sans-IO que se bombea a mano.
    socket: Option<UdpSocket>,
}

/// Espera máxima del handshake antes de rendirse.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Lectura máxima del socket por vuelta del modo bloqueante.
const RECV_SLICE: Duration = Duration::from_millis(20);

impl EnrollClient {
    /// Conecta al endpoint de enrolamiento y deja el cliente listo para el
    /// modo bloqueante: [`Self::send_token`] bombea el handshake y espera el
    /// resultado por su propio socket UDP.
    ///
    /// Para un banco de pruebas sans-IO que bombea datagramas a mano, usar
    /// [`Self::connect_sans_io`].
    pub fn connect(
        server_roots: Arc<RootCertStore>,
        server_pin: SpkiPin,
        now: Instant,
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<Self, EnrollError> {
        let mut client =
            Self::connect_sans_io(server_roots, server_pin, now, server_addr, server_name)?;
        let socket = UdpSocket::bind(("0.0.0.0", 0))
            .map_err(|error| EnrollError::Connect(format!("socket UDP: {error}")))?;
        socket
            .set_read_timeout(Some(RECV_SLICE))
            .map_err(|error| EnrollError::Connect(format!("timeout UDP: {error}")))?;
        client.socket = Some(socket);
        Ok(client)
    }

    /// Cliente sans-IO: el dueño bombea [`Self::handle_datagram`] y
    /// [`Self::poll_transmit`]. El token se envía con
    /// [`Self::send_token_async`].
    pub fn connect_sans_io(
        server_roots: Arc<RootCertStore>,
        server_pin: SpkiPin,
        now: Instant,
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<Self, EnrollError> {
        let verifier = PinnedServerVerifier::new(server_roots, server_pin)?;
        let mut tls = TlsClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN_ENROLL.to_vec()];
        tls.enable_early_data = false;
        let crypto =
            QuicClientConfig::try_from(tls).map_err(|error| EnrollError::Tls(error.to_string()))?;
        let mut quinn = QuinnClientConfig::new(Arc::new(crypto));
        quinn.transport_config(enroll_transport());
        let mut endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None);
        let (handle, conn) = endpoint
            .connect(now, quinn, server_addr, server_name)
            .map_err(|error| EnrollError::Connect(error.to_string()))?;
        Ok(Self {
            endpoint,
            conn,
            handle,
            ready: false,
            events: VecDeque::new(),
            outgoing: VecDeque::new(),
            send_stream: None,
            rx: Vec::new(),
            scratch: Vec::new(),
            terminal: false,
            sent_identity: None,
            outcome: None,
            socket: None,
        })
    }

    /// Procesa un datagrama del servidor.
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        from: SocketAddr,
        payload: &[u8],
    ) -> Result<(), EnrollError> {
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
        self.pump(now)
    }

    /// Siguiente evento.
    pub fn next_event(&mut self) -> Option<EnrollmentEvent> {
        self.events.pop_front()
    }

    /// Siguiente datagrama a enviar.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        if let Some(item) = self.outgoing.pop_front() {
            return Some(item);
        }
        self.scratch.clear();
        let transmit = self.conn.poll_transmit(now, 1, &mut self.scratch)?;
        Some((transmit.destination, self.scratch[..transmit.size].to_vec()))
    }

    /// `true` cuando el handshake terminó y el ALPN es `openrelay-enroll/1`.
    pub fn is_ready(&self) -> bool {
        self.ready && !self.terminal
    }

    /// Envía el token de la ventana junto con el SPKI DER de la clave que el
    /// cliente usará en mTLS y espera la decisión del servidor.
    ///
    /// Es la API de producto: requiere un cliente creado con [`Self::connect`]
    /// (socket propio). Bombea el handshake hasta `Ready`, escribe el
    /// `enroll.request` y espera el `enroll.result` hasta el TTL máximo de la
    /// ventana más un margen. Un rechazo local o una expiración devuelve
    /// [`EnrollError::Rejected`].
    pub fn send_token(
        &mut self,
        token: &EnrollmentToken,
        client_spki_der: &[u8],
    ) -> Result<EnrollOutcome, EnrollError> {
        if self.terminal || (self.socket.is_none() && !self.ready) {
            return Err(EnrollError::NotReady);
        }
        if self.socket.is_none() {
            return Err(EnrollError::Connect(
                "sans-IO client: use send_token_async and pump the datagrams".to_owned(),
            ));
        }
        let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while !self.is_ready() {
            if self.terminal {
                return Err(EnrollError::Closed);
            }
            if Instant::now() >= handshake_deadline {
                return Err(EnrollError::Timeout);
            }
            self.drive()?;
        }
        self.send_token_async(token, client_spki_der)?;
        let result_deadline = Instant::now() + MAX_ENROLL_TTL + Duration::from_secs(30);
        loop {
            while let Some(event) = self.next_event() {
                match event {
                    EnrollmentEvent::Accepted(_) => {
                        return self.outcome.take().ok_or(EnrollError::Malformed)
                    }
                    EnrollmentEvent::Rejected(reason) => return Err(EnrollError::Rejected(reason)),
                    EnrollmentEvent::Closed => return Err(EnrollError::Closed),
                    _ => {}
                }
            }
            if self.terminal {
                return Err(EnrollError::Closed);
            }
            if Instant::now() >= result_deadline {
                return Err(EnrollError::Timeout);
            }
            self.drive()?;
        }
    }

    /// Envía el token sin esperar el resultado. El dueño debe bombear el
    /// cliente y leer [`EnrollmentEvent::Accepted`]; después
    /// [`Self::outcome`] devuelve la hoja y la CA.
    pub fn send_token_async(
        &mut self,
        token: &EnrollmentToken,
        client_spki_der: &[u8],
    ) -> Result<(), EnrollError> {
        if !self.ready {
            return Err(EnrollError::NotReady);
        }
        let value = json!({
            "type":"enroll.request",
            "version":ENROLL_VERSION,
            "token": token.to_hex(),
            "spki": to_hex(client_spki_der)
        });
        self.sent_identity = Some(SpkiPin::from_spki_der(client_spki_der));
        self.write_frame(&value)
    }

    /// Resultado aceptado, si el servidor ya respondió.
    pub fn outcome(&self) -> Option<&EnrollOutcome> {
        self.outcome.as_ref()
    }

    /// Una vuelta del modo bloqueante: temporizadores, salida y una lectura.
    fn drive(&mut self) -> Result<(), EnrollError> {
        let now = Instant::now();
        self.conn.handle_timeout(now);
        let socket = self
            .socket
            .take()
            .ok_or(EnrollError::Connect("client without socket".to_owned()))?;
        while let Some((to, bytes)) = self.poll_transmit(now) {
            socket
                .send_to(&bytes, to)
                .map_err(|error| EnrollError::Connect(format!("UDP send: {error}")))?;
        }
        let mut buf = [0u8; 2048];
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                self.socket = Some(socket);
                self.handle_datagram(Instant::now(), from, &buf[..len])?;
                return Ok(());
            }
            Err(error) if is_timeout(&error) => {}
            Err(error) => {
                self.socket = Some(socket);
                return Err(EnrollError::Connect(format!("lectura UDP: {error}")));
            }
        }
        self.socket = Some(socket);
        Ok(())
    }

    /// Envía un frame de sondeo negativo (audio/control/ticket o JSON
    /// arbitrario). No forma parte del flujo de producto: existe para probar
    /// que la frontera rechaza superficies prohibidas.
    #[doc(hidden)]
    pub fn send_unchecked_frame(&mut self, value: &Value) -> Result<(), EnrollError> {
        if !self.ready {
            return Err(EnrollError::NotReady);
        }
        self.write_frame(value)
    }

    fn write_frame(&mut self, value: &Value) -> Result<(), EnrollError> {
        let frame = encode_frame(value)?;
        let stream = match self.send_stream {
            Some(stream) => stream,
            None => {
                let stream = self
                    .conn
                    .streams()
                    .open(Dir::Bi)
                    .ok_or(EnrollError::Stream("no enrollment stream"))?;
                self.send_stream = Some(stream);
                stream
            }
        };
        let mut written = 0usize;
        while written < frame.len() {
            match self.conn.send_stream(stream).write(&frame[written..]) {
                Ok(0) | Err(_) => return Err(EnrollError::Stream("write rejected")),
                Ok(count) => written += count,
            }
        }
        Ok(())
    }

    /// Cierra la conexión real y deja el cliente definitivamente no listo.
    ///
    /// Idempotente: el rechazo se registra una sola vez y un `Ready` o
    /// `Accepted` posterior ya no puede reaparecer. No toca `outgoing`, de modo
    /// que un `CONNECTION_CLOSE` ya encolado no se pierde.
    fn fail_closed(&mut self, now: Instant, code: u32) {
        self.ready = false;
        self.send_stream = None;
        self.rx.clear();
        if !self.terminal {
            self.terminal = true;
            self.events
                .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed));
        }
        self.conn.close(
            now,
            VarInt::from_u32(code),
            Bytes::from_static(b"enroll malformed"),
        );
    }

    fn pump(&mut self, now: Instant) -> Result<(), EnrollError> {
        while let Some(event) = self.conn.poll_endpoint_events() {
            if let Some(event) = self.endpoint.handle_event(self.handle, event) {
                self.conn.handle_event(event);
            }
        }
        while let Some(event) = self.conn.poll() {
            match event {
                Event::Connected => {
                    let handshake = self
                        .conn
                        .crypto_session()
                        .handshake_data()
                        .and_then(|data| data.downcast::<HandshakeData>().ok());
                    if handshake.and_then(|data| data.protocol).as_deref() != Some(ALPN_ENROLL) {
                        self.fail_closed(now, 0x208);
                        return Ok(());
                    }
                    // Un `Connected` tardío no revive un cliente ya terminal.
                    if !self.terminal {
                        self.ready = true;
                        self.events.push_back(EnrollmentEvent::Ready);
                    }
                }
                Event::ConnectionLost { .. } => {
                    // La conexión ya no existe: la readiness no puede volver,
                    // aunque llegue un `Connected` o un `Readable` tardío.
                    self.ready = false;
                    self.terminal = true;
                    self.events.push_back(EnrollmentEvent::Closed);
                    return Ok(());
                }
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                    // El servidor responde en el stream abierto por el cliente;
                    // no abre streams propios.
                }
                Event::Stream(StreamEvent::Readable { id }) => {
                    if self.terminal {
                        continue;
                    }
                    self.read_result(now, id);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_result(&mut self, now: Instant, id: StreamId) {
        loop {
            let (data, overflow, fin) = {
                let mut recv = self.conn.recv_stream(id);
                let Ok(mut chunks) = recv.read(true) else {
                    return;
                };
                let mut data = Vec::new();
                let mut overflow = false;
                let mut fin = false;
                loop {
                    match chunks.next(usize::MAX) {
                        Ok(Some(chunk)) => {
                            if self.rx.len() + data.len() + chunk.bytes.len()
                                > MAX_ENROLL_PAYLOAD + 4
                            {
                                overflow = true;
                                break;
                            }
                            data.extend_from_slice(&chunk.bytes);
                        }
                        // `Ok(None)` es EOF real del FIN contiguo ya consumido; no
                        // confundirlo con `Blocked`, que es un hueco aún pendiente.
                        Ok(None) => {
                            fin = true;
                            break;
                        }
                        // `Blocked`: sin más datos por ahora; se conserva el prefijo.
                        Err(ReadError::Blocked) => break,
                        Err(ReadError::Reset(_)) => {
                            overflow = true;
                            break;
                        }
                    }
                }
                let _ = chunks.finalize();
                (data, overflow, fin)
            };
            if overflow {
                self.fail_closed(now, 0x203);
                return;
            }
            self.rx.extend_from_slice(&data);
            while self.rx.len() >= 4 {
                let len =
                    u32::from_be_bytes([self.rx[0], self.rx[1], self.rx[2], self.rx[3]]) as usize;
                if len == 0 || len > MAX_ENROLL_PAYLOAD {
                    self.fail_closed(now, 0x205);
                    return;
                }
                if self.rx.len() < 4 + len {
                    break;
                }
                let payload = self.rx[4..4 + len].to_vec();
                self.rx.drain(..4 + len);
                self.evaluate(&payload);
            }
            // EOF real con bytes sin consumir: respuesta truncada. Fail-closed:
            // además de rechazar, cierra la conexión real del cliente.
            if fin && !self.rx.is_empty() {
                self.fail_closed(now, 0x204);
                return;
            }
            if data.is_empty() {
                return;
            }
        }
    }

    fn evaluate(&mut self, payload: &[u8]) {
        let Ok(value) = serde_json::from_slice::<Value>(payload) else {
            self.events
                .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed));
            return;
        };
        if parse_type(&value) != Some("enroll.result")
            || parse_version(&value) != Some(ENROLL_VERSION)
        {
            self.events
                .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed));
            return;
        }
        match value.get("status").and_then(Value::as_str) {
            Some("accepted") => match self.sent_identity {
                Some(identity) => {
                    let leaf = value
                        .get("client_leaf")
                        .and_then(Value::as_str)
                        .and_then(decode_hex);
                    let ca = value
                        .get("session_ca")
                        .and_then(Value::as_str)
                        .and_then(decode_hex);
                    let port = value
                        .get("session_port")
                        .and_then(Value::as_u64)
                        .and_then(|port| u16::try_from(port).ok());
                    match (leaf, ca, port) {
                        (Some(client_leaf_der), Some(session_ca_der), Some(session_port)) => {
                            self.outcome = Some(EnrollOutcome {
                                client_leaf_der,
                                session_ca_der,
                                session_port,
                            });
                            self.events.push_back(EnrollmentEvent::Accepted(identity));
                        }
                        _ => self
                            .events
                            .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed)),
                    }
                }
                None => self
                    .events
                    .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed)),
            },
            Some("rejected") => {
                let reason = match value.get("reason").and_then(Value::as_str) {
                    Some("expired") => EnrollReject::Expired,
                    Some("reused") => EnrollReject::Reused,
                    Some("mismatch") => EnrollReject::Mismatch,
                    Some("no_window") => EnrollReject::NoWindow,
                    Some("version") => EnrollReject::Version,
                    _ => EnrollReject::Malformed,
                };
                self.events.push_back(EnrollmentEvent::Rejected(reason));
            }
            _ => self
                .events
                .push_back(EnrollmentEvent::Rejected(EnrollReject::Malformed)),
        }
    }
}

// El identificador de conexión no se expone; el cliente y el servidor de
// enrolamiento viven una sola conexión cada uno.

#[cfg(test)]
#[path = "enrollment_regressions.rs"]
mod enrollment_regressions;
