//! Superficie de sesión: QUIC mTLS con ACL *default deny*.
//!
//! El servidor exige certificado de cliente (cadena contra la CA local), ALPN
//! `arl-session/1`, identidad de peer presente y una entrada explícita en la
//! [`Acl`]. Hasta que todo eso se cumple no se lee ni se entrega ningún
//! DATAGRAM de audio, stream de control o ticket. El cliente exige cadena
//! contra la CA local **y** pin SPKI del servidor antes de enviar datos.
//!
//! El crate es sans-IO: [`SessionServer`] y [`SessionClient`] reciben y
//! devuelven datagramas UDP. Los tests usan un enlace en memoria; el socket de
//! producción se cablea en T13/T14 sin cambiar esta frontera.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arl_audio_core::{AudioFrame, FrameError};
use arl_proto::wire::{self, ControlMessage, WireError, CONTROL_LENGTH_BYTES};
use bytes::{Bytes, BytesMut};
use quinn_proto::crypto::rustls::{HandshakeData, QuicClientConfig, QuicServerConfig};
use quinn_proto::{
    ClientConfig as QuinnClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint,
    EndpointConfig, Event, Incoming, ReadError, ServerConfig as QuinnServerConfig, StreamEvent,
    StreamId, TransportConfig, VarInt, WriteError,
};
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig as TlsClientConfig, RootCertStore, ServerConfig as TlsServerConfig};

use crate::identity::{
    IdentityError, PinnedServerVerifier, SpkiPin, StaticClientCertResolver,
    StaticServerCertResolver, MAX_CERT_CHAIN,
};
use crate::{ALPN_SESSION, MAX_AUDIO_DATAGRAM, MAX_CONTROL_FRAME};

/// Conexiones de sesión simultáneas máximas por endpoint.
pub const MAX_SESSIONS: usize = 8;
/// Handshakes pendientes máximos antes de aceptar.
pub const MAX_PENDING_INCOMING: usize = 8;
/// Audio en cola por sesión antes de descartar el más viejo.
pub const MAX_QUEUED_AUDIO: usize = 8;
/// Mensajes de control en cola por sesión.
pub const MAX_QUEUED_CONTROL: usize = 8;
/// Timeout de inactividad por defecto.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

// Códigos de cierre de la sesión. Son parte del comportamiento observable del
// transporte: el peer distingue un cierre por framing corrupto del cierre por
// autorización. Los motivos son constantes y nunca incluyen payload del frame.
/// Cierre por framing de control corrupto (prefijo fuera de `1..=16384`,
/// frame que excede el límite o stream de control interrumpido a mitad de frame).
const CLOSE_CONTROL_FRAMING: u32 = 0x104;
/// Cierre por DATAGRAM de audio fuera de límite.
const CLOSE_AUDIO_OVERSIZED: u32 = 0x101;
/// Motivo (nunca payload) de cierre cuando una respuesta pendiente no puede
/// escribirse por un fallo permanente del stream, no por contrapresión.
const REPLY_FAILED: &str = "no se pudo responder el error de control";
/// Cierre por revocación de la identidad mientras la sesión está viva.
const CLOSE_REVOKED: u32 = 0x105;
/// Motivo (nunca payload) de cierre cuando un envío público de `send_control`
/// no puede completarse por un fallo permanente del stream de control.
const SEND_FAILED: &str = "no se pudo enviar el mensaje de control";

/// Límites explícitos de una sesión.
#[derive(Debug, Clone)]
pub struct SessionLimits {
    /// Timeout de inactividad QUIC.
    pub idle_timeout: Duration,
    /// Frame de control máximo **total**, prefijo `u32` incluido; nunca
    /// superior a [`MAX_CONTROL_FRAME`] (= payload máximo + prefijo). El
    /// payload útil queda por tanto en `max_control_frame - 4`.
    pub max_control_frame: usize,
    /// Datagrama de audio máximo; nunca superior a [`MAX_AUDIO_DATAGRAM`].
    pub max_audio_datagram: usize,
    /// Streams bidireccionales concurrentes.
    pub max_control_streams: u32,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            max_control_frame: MAX_CONTROL_FRAME,
            max_audio_datagram: MAX_AUDIO_DATAGRAM,
            max_control_streams: 2,
        }
    }
}

impl SessionLimits {
    fn validate(&self) -> Result<(), TransportError> {
        if self.idle_timeout.is_zero() || self.idle_timeout > Duration::from_secs(60) {
            return Err(TransportError::Config("idle_timeout fuera de 1..=60 s"));
        }
        // El campo es el frame **total**: con menos bytes que el prefijo no
        // cabe ningún frame válido (payload mínimo 1).
        if self.max_control_frame <= CONTROL_LENGTH_BYTES
            || self.max_control_frame > MAX_CONTROL_FRAME
        {
            return Err(TransportError::Config(
                "max_control_frame fuera de 5..=16388 (total, prefijo incluido)",
            ));
        }
        if self.max_audio_datagram == 0 || self.max_audio_datagram > MAX_AUDIO_DATAGRAM {
            return Err(TransportError::Config("max_audio_datagram fuera de límite"));
        }
        if self.max_control_streams == 0 || self.max_control_streams > 8 {
            return Err(TransportError::Config("max_control_streams fuera de 1..=8"));
        }
        Ok(())
    }
}

/// Capacidades concedidas a una identidad. Default deny: [`Self::none`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    mic: bool,
    control_read: bool,
    control_write: bool,
    return_audio: bool,
}

impl Capabilities {
    /// Sin capacidades.
    pub fn none() -> Self {
        Self::default()
    }

    /// Concede micrófono (audio cliente → servidor).
    pub fn with_mic(mut self) -> Self {
        self.mic = true;
        self
    }

    /// Concede lectura de control (servidor → cliente).
    pub fn with_control_read(mut self) -> Self {
        self.control_read = true;
        self
    }

    /// Concede escritura de control (cliente → servidor).
    pub fn with_control_write(mut self) -> Self {
        self.control_write = true;
        self
    }

    /// Concede audio de retorno (servidor → cliente; futuro).
    pub fn with_return_audio(mut self) -> Self {
        self.return_audio = true;
        self
    }

    /// ¿Puede enviar micrófono?
    pub fn mic(&self) -> bool {
        self.mic
    }

    /// ¿Puede leer control?
    pub fn control_read(&self) -> bool {
        self.control_read
    }

    /// ¿Puede escribir control?
    pub fn control_write(&self) -> bool {
        self.control_write
    }

    /// ¿Puede recibir audio de retorno?
    pub fn return_audio(&self) -> bool {
        self.return_audio
    }
}

/// ACL indexada por pin SPKI de la identidad del cliente.
#[derive(Debug, Default)]
pub struct Acl {
    entries: HashMap<SpkiPin, Capabilities>,
}

impl Acl {
    /// ACL vacía: toda identidad queda denegada.
    pub fn new() -> Self {
        Self::default()
    }

    /// Concede capacidades a una identidad concreta.
    pub fn allow(&mut self, pin: SpkiPin, capabilities: Capabilities) -> &mut Self {
        self.entries.insert(pin, capabilities);
        self
    }

    /// Capacidades de una identidad, si existe.
    pub fn capabilities(&self, pin: &SpkiPin) -> Option<Capabilities> {
        self.entries.get(pin).copied()
    }

    /// Quita una identidad: sus handshakes futuros quedan denegados.
    fn remove(&mut self, pin: &SpkiPin) {
        self.entries.remove(pin);
    }

    /// Identidades registradas.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// ¿Está vacía?
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Identificador opaco de una sesión autorizada.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(u64);

impl SessionId {
    /// Valor numérico para logs y telemetría.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Motivo tipado de rechazo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectReason {
    /// El handshake TLS no terminó o no dejó datos.
    Tls,
    /// ALPN distinto de `arl-session/1`.
    Alpn,
    /// El cliente no presentó certificado.
    NoPeerCertificate,
    /// El certificado del peer no se pudo interpretar.
    MalformedIdentity,
    /// Identidad válida sin entrada en la ACL (default deny).
    NoAclEntry,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Tls => "handshake TLS incompleto",
            Self::Alpn => "ALPN distinto de arl-session/1",
            Self::NoPeerCertificate => "cliente sin certificado",
            Self::MalformedIdentity => "identidad de peer malformada",
            Self::NoAclEntry => "identidad sin entrada en la ACL",
        };
        f.write_str(text)
    }
}

/// Evento del servidor de sesión.
#[derive(Debug)]
pub enum ServerEvent {
    /// Sesión autorizada; desde este punto se entregan datos.
    Authorized {
        /// Identificador opaco.
        session: SessionId,
        /// Identidad del cliente (pin SPKI).
        identity: SpkiPin,
        /// Capacidades concedidas.
        capabilities: Capabilities,
    },
    /// Conexión rechazada antes de cualquier dato de aplicación.
    Rejected {
        /// Motivo.
        reason: RejectReason,
    },
    /// Sesión cerrada o perdida.
    Closed {
        /// Identificador opaco.
        session: SessionId,
    },
}

/// Evento del cliente de sesión.
#[derive(Debug)]
pub enum ClientEvent {
    /// Handshake completo, ALPN y pin del servidor verificados.
    Authorized {
        /// Pin SPKI del servidor presentado.
        server: SpkiPin,
    },
    /// El servidor o el handshake no superaron la frontera.
    Rejected(RejectReason),
    /// Conexión cerrada o perdida.
    Closed,
}

/// Error de transporte, sin material secreto.
#[derive(Debug)]
#[non_exhaustive]
pub enum TransportError {
    /// Configuración inválida.
    Config(&'static str),
    /// Identidad o certificado inválido.
    Identity(IdentityError),
    /// rustls rechazó la configuración TLS.
    Tls(String),
    /// No se pudo iniciar la conexión.
    Connect(String),
    /// La sesión no está autorizada.
    Unauthorized,
    /// Operación sobre una sesión cerrada o inexistente.
    Closed,
    /// Capacidad insuficiente en la ACL.
    Capability(&'static str),
    /// Frame de audio inválido.
    Audio(FrameError),
    /// Frame de control inválido.
    Control(WireError),
    /// DATAGRAM rechazado por quinn.
    Datagram(&'static str),
    /// Operación de stream rechazada.
    Stream(&'static str),
    /// Cola acotada llena.
    QueueFull,
    /// Violación de protocolo.
    Protocol(&'static str),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(reason) => write!(f, "configuración inválida: {reason}"),
            Self::Identity(error) => write!(f, "identidad inválida: {error}"),
            Self::Tls(detail) => write!(f, "TLS inválido: {detail}"),
            Self::Connect(detail) => write!(f, "conexión fallida: {detail}"),
            Self::Unauthorized => write!(f, "sesión no autorizada"),
            Self::Closed => write!(f, "sesión cerrada"),
            Self::Capability(cap) => write!(f, "capacidad ausente: {cap}"),
            Self::Audio(error) => write!(f, "audio inválido: {error}"),
            Self::Control(error) => write!(f, "control inválido: {error:?}"),
            Self::Datagram(reason) => write!(f, "DATAGRAM rechazado: {reason}"),
            Self::Stream(reason) => write!(f, "stream rechazado: {reason}"),
            Self::QueueFull => write!(f, "cola acotada llena"),
            Self::Protocol(reason) => write!(f, "protocolo: {reason}"),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<IdentityError> for TransportError {
    fn from(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}

fn transport_config(
    limits: &SessionLimits,
    datagrams: bool,
) -> Result<Arc<TransportConfig>, TransportError> {
    let mut config = TransportConfig::default();
    config.max_idle_timeout(Some(
        quinn_proto::IdleTimeout::try_from(limits.idle_timeout)
            .map_err(|_| TransportError::Config("idle_timeout inválido"))?,
    ));
    config.max_concurrent_bidi_streams(limits.max_control_streams.into());
    config.max_concurrent_uni_streams(0u32.into());
    config.stream_receive_window(32_768u32.into());
    config.receive_window(131_072u32.into());
    if datagrams {
        config.datagram_receive_buffer_size(Some(4_096));
        config.datagram_send_buffer_size(4_096);
    } else {
        config.datagram_receive_buffer_size(None);
        config.datagram_send_buffer_size(0);
    }
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct Slot {
    conn: Connection,
    identity: Option<SpkiPin>,
    capabilities: Capabilities,
    authorized: bool,
    audio: VecDeque<AudioFrame>,
    control: VecDeque<ControlMessage>,
    recv_streams: HashSet<StreamId>,
    send_stream: Option<StreamId>,
    /// Prefijo parcial **por stream**: un frame a medio llegar en un stream no
    /// puede completarse con bytes de otro `StreamId`. Cada entrada está
    /// acotada por `max_control_frame` (total, prefijo incluido).
    control_rx: ControlBuffers,
    /// Respuestas `error` aceptadas por QUIC y aún no escritas del todo, más los
    /// envíos públicos de `send_control`. Ambos comparten esta cola por
    /// `StreamId` para no intercalar prefijos ni reiniciar offsets.
    control_tx: PendingReplies,
    /// `send_control` sufrió un fallo **permanente** del stream de control (no
    /// contrapresión): el framing ya no es recuperable y el siguiente `pump`
    /// cierra la sesión.
    tx_failed: bool,
    closed: bool,
    /// `ServerEvent::Closed` ya emitido para esta sesión: ni el cierre por
    /// `ConnectionLost` ni la retirada al drenar pueden emitirlo dos veces.
    close_announced: bool,
}

/// Servidor de sesión sans-IO.
pub struct SessionServer {
    endpoint: Endpoint,
    accept_config: Arc<QuinnServerConfig>,
    acl: Acl,
    limits: SessionLimits,
    pending: VecDeque<Incoming>,
    slots: HashMap<SessionId, Slot>,
    by_handle: HashMap<ConnectionHandle, SessionId>,
    events: VecDeque<ServerEvent>,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
    next_id: u64,
    scratch: Vec<u8>,
}

impl SessionServer {
    /// Construye el servidor con la identidad del daemon, la CA que firma
    /// clientes y la ACL inicial (vacía = todo denegado).
    pub fn new(
        identity: Arc<CertifiedKey>,
        client_roots: Arc<RootCertStore>,
        acl: Acl,
        limits: SessionLimits,
    ) -> Result<Self, TransportError> {
        limits.validate()?;
        if identity.cert.len() > MAX_CERT_CHAIN {
            return Err(TransportError::Identity(IdentityError::ChainTooLong));
        }
        let verifier = WebPkiClientVerifier::builder(client_roots)
            .build()
            .map_err(|_| TransportError::Config("verificador de cliente inválido"))?;
        let mut tls = TlsServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(Arc::new(StaticServerCertResolver::new(identity)));
        tls.alpn_protocols = vec![ALPN_SESSION.to_vec()];
        tls.send_tls13_tickets = 0;
        tls.max_early_data_size = 0;
        let crypto = QuicServerConfig::try_from(tls)
            .map_err(|error| TransportError::Tls(error.to_string()))?;
        let mut quinn = QuinnServerConfig::with_crypto(Arc::new(crypto));
        quinn.transport_config(transport_config(&limits, true)?);
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
            acl,
            limits,
            pending: VecDeque::new(),
            slots: HashMap::new(),
            by_handle: HashMap::new(),
            events: VecDeque::new(),
            outgoing: VecDeque::new(),
            next_id: 1,
            scratch: Vec::new(),
        })
    }

    /// Procesa un datagrama recibido del peer.
    pub fn handle_datagram(
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
            Some(DatagramEvent::NewConnection(incoming)) => {
                if self.pending.len() >= MAX_PENDING_INCOMING {
                    return Err(TransportError::QueueFull);
                }
                self.pending.push_back(incoming);
            }
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(session) = self.by_handle.get(&handle).copied() {
                    if let Some(slot) = self.slots.get_mut(&session) {
                        slot.conn.handle_event(event);
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

    /// Siguiente evento del servidor.
    pub fn next_event(&mut self) -> Option<ServerEvent> {
        self.events.pop_front()
    }

    /// Siguiente datagrama a enviar.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        if let Some(item) = self.outgoing.pop_front() {
            return Some(item);
        }
        let ids: Vec<SessionId> = self.slots.keys().copied().collect();
        for id in ids {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            self.scratch.clear();
            if let Some(transmit) = slot.conn.poll_transmit(now, 1, &mut self.scratch) {
                let bytes = self.scratch[..transmit.size].to_vec();
                return Some((transmit.destination, bytes));
            }
        }
        None
    }

    /// Próximo timeout de conexión, si hay alguno.
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        self.slots
            .values_mut()
            .filter_map(|slot| slot.conn.poll_timeout())
            .min()
    }

    /// Despierta timeouts vencidos.
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), TransportError> {
        let ids: Vec<SessionId> = self.slots.keys().copied().collect();
        for id in ids {
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.conn.handle_timeout(now);
            }
        }
        self.pump(now)
    }

    /// Identidad autorizada de una sesión.
    pub fn identity(&self, session: SessionId) -> Option<SpkiPin> {
        self.slots
            .get(&session)
            .filter(|slot| slot.authorized)
            .and_then(|slot| slot.identity)
    }

    /// Capacidades de una sesión autorizada.
    pub fn capabilities(&self, session: SessionId) -> Option<Capabilities> {
        self.slots
            .get(&session)
            .filter(|slot| slot.authorized)
            .map(|slot| slot.capabilities)
    }

    /// Concede capacidades a una identidad en caliente.
    pub fn allow(&mut self, pin: SpkiPin, capabilities: Capabilities) {
        self.acl.allow(pin, capabilities);
    }

    /// Revoca una identidad: la quita de la ACL y cierra con `CLOSE_REVOKED`
    /// sus sesiones vivas, emitiendo `ServerEvent::Closed` una vez por sesión.
    pub fn revoke(&mut self, pin: &SpkiPin, now: Instant) {
        self.acl.remove(pin);
        let ids: Vec<SessionId> = self
            .slots
            .iter()
            .filter(|(_, slot)| slot.identity == Some(*pin))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            if !slot.closed {
                slot.conn.close(
                    now,
                    VarInt::from_u32(CLOSE_REVOKED),
                    Bytes::from_static(b"revoked"),
                );
            }
            slot.closed = true;
            slot.revoke();
            if slot.announce_close() {
                self.events.push_back(ServerEvent::Closed { session: id });
            }
        }
    }

    /// Envía un frame de audio a un cliente con capacidad `return_audio`.
    pub fn send_audio(
        &mut self,
        session: SessionId,
        frame: &AudioFrame,
    ) -> Result<(), TransportError> {
        let wire = frame.encode();
        if wire.len() > self.limits.max_audio_datagram {
            return Err(TransportError::Audio(FrameError::Oversized {
                len: wire.len(),
            }));
        }
        let slot = self.authorized_slot(session, Capabilities::return_audio, "return_audio")?;
        slot.conn
            .datagrams()
            .send(Bytes::copy_from_slice(&wire), true)
            .map_err(|_| TransportError::Datagram("audio"))
    }

    /// Recibe el siguiente frame de audio de una sesión con capacidad `mic`.
    pub fn recv_audio(&mut self, session: SessionId) -> Option<AudioFrame> {
        self.slots
            .get_mut(&session)
            .filter(|slot| slot.authorized && slot.capabilities.mic)
            .and_then(|slot| slot.audio.pop_front())
    }

    /// Envía un mensaje de control a una sesión con capacidad `control_read`.
    ///
    /// Codificación, tamaño, sesión cerrada, falta de autorización o de
    /// capacidad se comprueban antes de encolar: esos `Err` no escriben bytes
    /// nuevos. `QueueFull` también se decide antes de escribir, y el
    /// `Stream(..)` de no poder abrir el stream de control ocurre igualmente
    /// antes de aceptar el frame.
    ///
    /// Contrato de resultado:
    /// - `Ok(())`: el frame **íntegro** quedó aceptado en la cola de salida
    ///   acotada y se entrega de forma **asíncrona**; el offset ya escrito se
    ///   conserva y el resto se reintenta en cada `Writable`/pump. No promete
    ///   entrega síncrona ni recepción por el peer.
    /// - `Err(QueueFull)`: la cola no admite otro frame. Se decide antes de
    ///   escribir, así que este frame no deja bytes nuevos.
    /// - `Err(Stream(..))` al vaciar la cola: fallo **permanente** del stream
    ///   (reset, stopped, desconocido). No garantiza cero bytes: el frame
    ///   aceptado pudo dejar un prefijo escrito. La cola se descarta,
    ///   `tx_failed` queda marcado y el siguiente `pump` cierra la sesión; no
    ///   se reintenta ni se sigue usando la sesión como si estuviera limpia.
    ///   La contrapresión (`Blocked`) nunca es un `Err`: se reintenta.
    pub fn send_control(
        &mut self,
        session: SessionId,
        message: &ControlMessage,
    ) -> Result<(), TransportError> {
        let frame = wire::encode_message(message).map_err(TransportError::Control)?;
        if frame.len() > self.limits.max_control_frame {
            return Err(TransportError::Control(WireError::invalid(None)));
        }
        let slot = self.authorized_slot(session, Capabilities::control_read, "control.read")?;
        let stream = match slot.send_stream {
            Some(stream) => stream,
            None => {
                let stream = slot
                    .conn
                    .streams()
                    .open(Dir::Bi)
                    .ok_or(TransportError::Stream("sin stream de control"))?;
                slot.send_stream = Some(stream);
                stream
            }
        };
        match send_queued(&mut slot.control_tx, &mut slot.conn, stream, frame) {
            Ok(()) => Ok(()),
            Err(error) => {
                // `QueueFull` se decidió antes de escribir un solo byte. Un
                // fallo permanente del stream sí puede haber dejado un prefijo:
                // el framing ya no es recuperable, así que la sesión se cierra
                // en el siguiente `pump`, que es donde hay un `Instant`.
                if matches!(error, TransportError::Stream(_)) {
                    slot.tx_failed = true;
                }
                Err(error)
            }
        }
    }

    /// Recibe el siguiente mensaje de control de una sesión con capacidad
    /// `control_write`.
    pub fn recv_control(&mut self, session: SessionId) -> Option<ControlMessage> {
        self.slots
            .get_mut(&session)
            .filter(|slot| slot.authorized && slot.capabilities.control_write)
            .and_then(|slot| slot.control.pop_front())
    }

    fn authorized_slot(
        &mut self,
        session: SessionId,
        required: fn(&Capabilities) -> bool,
        cap: &'static str,
    ) -> Result<&mut Slot, TransportError> {
        let slot = self.slots.get_mut(&session).ok_or(TransportError::Closed)?;
        if !slot.authorized {
            return Err(TransportError::Unauthorized);
        }
        if !required(&slot.capabilities) {
            return Err(TransportError::Capability(cap));
        }
        Ok(slot)
    }

    fn authorize(&self, conn: &Connection) -> Result<(SpkiPin, Capabilities), RejectReason> {
        let handshake = conn
            .crypto_session()
            .handshake_data()
            .ok_or(RejectReason::Tls)?;
        let handshake = handshake
            .downcast::<HandshakeData>()
            .map_err(|_| RejectReason::Tls)?;
        if handshake.protocol.as_deref() != Some(ALPN_SESSION) {
            return Err(RejectReason::Alpn);
        }
        let peer = conn
            .crypto_session()
            .peer_identity()
            .ok_or(RejectReason::NoPeerCertificate)?;
        let chain = peer
            .downcast::<Vec<CertificateDer<'static>>>()
            .map_err(|_| RejectReason::MalformedIdentity)?;
        let leaf = chain.first().ok_or(RejectReason::NoPeerCertificate)?;
        let pin = SpkiPin::from_certificate_der(leaf.as_ref())
            .map_err(|_| RejectReason::MalformedIdentity)?;
        let capabilities = self
            .acl
            .capabilities(&pin)
            .ok_or(RejectReason::NoAclEntry)?;
        Ok((pin, capabilities))
    }

    fn pump(&mut self, now: Instant) -> Result<(), TransportError> {
        while self.slots.len() < MAX_SESSIONS {
            let Some(incoming) = self.pending.pop_front() else {
                break;
            };
            let mut response = Vec::new();
            match self.endpoint.accept(
                incoming,
                now,
                &mut response,
                Some(Arc::clone(&self.accept_config)),
            ) {
                Ok((handle, conn)) => {
                    let id = SessionId(self.next_id);
                    self.next_id += 1;
                    self.by_handle.insert(handle, id);
                    self.slots.insert(
                        id,
                        Slot {
                            conn,
                            identity: None,
                            capabilities: Capabilities::none(),
                            authorized: false,
                            audio: VecDeque::new(),
                            control: VecDeque::new(),
                            recv_streams: HashSet::new(),
                            send_stream: None,
                            control_rx: ControlBuffers::default(),
                            control_tx: PendingReplies::default(),
                            tx_failed: false,
                            closed: false,
                            close_announced: false,
                        },
                    );
                }
                Err(error) => {
                    if let Some(transmit) = error.response {
                        self.outgoing
                            .push_back((transmit.destination, response[..transmit.size].to_vec()));
                    }
                }
            }
        }

        let ids: Vec<SessionId> = self.slots.keys().copied().collect();
        for id in ids {
            let Some(mut slot) = self.slots.remove(&id) else {
                continue;
            };
            self.pump_slot(id, &mut slot, now);
            if slot.closed && slot.conn.is_drained() {
                self.by_handle.retain(|_, value| *value != id);
                if slot.announce_close() {
                    self.events.push_back(ServerEvent::Closed { session: id });
                }
            } else {
                self.slots.insert(id, slot);
            }
        }
        Ok(())
    }

    fn pump_slot(&mut self, id: SessionId, slot: &mut Slot, now: Instant) {
        while let Some(event) = slot.conn.poll_endpoint_events() {
            let handle = self
                .by_handle
                .iter()
                .find_map(|(handle, value)| (*value == id).then_some(*handle));
            if let Some(handle) = handle {
                if let Some(event) = self.endpoint.handle_event(handle, event) {
                    slot.conn.handle_event(event);
                }
            }
        }

        while let Some(event) = slot.conn.poll() {
            match event {
                Event::Connected => {
                    if slot.authorized {
                        continue;
                    }
                    match self.authorize(&slot.conn) {
                        Ok((pin, capabilities)) => {
                            slot.authorized = true;
                            slot.identity = Some(pin);
                            slot.capabilities = capabilities;
                            self.events.push_back(ServerEvent::Authorized {
                                session: id,
                                identity: pin,
                                capabilities,
                            });
                        }
                        Err(reason) => {
                            slot.conn.close(
                                now,
                                VarInt::from_u32(0x100),
                                Bytes::from_static(b"unauthorized"),
                            );
                            slot.closed = true;
                            self.events.push_back(ServerEvent::Rejected { reason });
                            return;
                        }
                    }
                }
                Event::ConnectionLost { .. } => {
                    // Pérdida remota: no queda estado autorizado ni residuo.
                    // No se re-cierra (`conn.close`) ni se emite `Closed` dos
                    // veces; `outgoing` conserva el CONNECTION_CLOSE del peer
                    // y el cierre local pendiente.
                    slot.closed = true;
                    slot.revoke();
                    if slot.announce_close() {
                        self.events.push_back(ServerEvent::Closed { session: id });
                    }
                    return;
                }
                Event::DatagramReceived => {
                    if !slot.authorized {
                        continue;
                    }
                    while let Some(datagram) = slot.conn.datagrams().recv() {
                        if datagram.len() > self.limits.max_audio_datagram {
                            slot.conn.close(
                                now,
                                VarInt::from_u32(CLOSE_AUDIO_OVERSIZED),
                                Bytes::from_static(b"oversized"),
                            );
                            slot.closed = true;
                            return;
                        }
                        match AudioFrame::decode(&datagram) {
                            Ok(frame) => {
                                // El DATAGRAM ya se recibió y se decodificó; el
                                // ingreso en la cola de aplicación exige `mic`,
                                // el mismo permiso que `recv_audio` para
                                // entregar. Sin él el audio se descarta en vez
                                // de retenerse.
                                if slot.capabilities.mic {
                                    if slot.audio.len() >= MAX_QUEUED_AUDIO {
                                        slot.audio.pop_front();
                                    }
                                    slot.audio.push_back(frame);
                                }
                            }
                            Err(error) => {
                                self.events.push_back(ServerEvent::Rejected {
                                    reason: RejectReason::Tls,
                                });
                                let _ = error;
                                slot.conn.close(
                                    now,
                                    VarInt::from_u32(0x102),
                                    Bytes::from_static(b"bad audio"),
                                );
                                slot.closed = true;
                                return;
                            }
                        }
                    }
                }
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                    if !slot.authorized {
                        slot.conn.close(
                            now,
                            VarInt::from_u32(0x103),
                            Bytes::from_static(b"unauthorized"),
                        );
                        slot.closed = true;
                        return;
                    }
                    while let Some(stream) = slot.conn.streams().accept(Dir::Bi) {
                        slot.recv_streams.insert(stream);
                    }
                }
                Event::Stream(StreamEvent::Readable { id: stream }) => {
                    if !slot.authorized {
                        continue;
                    }
                    slot.recv_streams.insert(stream);
                }
                Event::Stream(StreamEvent::Writable { .. }) => {
                    // Crédito liberado: es el momento de reanudar la respuesta
                    // `error` que quedó pendiente por `Blocked`.
                    if flush_replies(slot).is_err() {
                        slot.fail_close(now, CLOSE_CONTROL_FRAMING, REPLY_FAILED);
                        return;
                    }
                }
                Event::Stream(StreamEvent::Finished { .. })
                | Event::Stream(StreamEvent::Stopped { .. }) => {
                    // Eventos del medio de envío; el cierre del lado de lectura
                    // del peer se observa como `Ok(None)` en `read_control`.
                }
                _ => {}
            }
            // Tras cada evento se atienden todos los streams de control
            // conocidos: un `Opened` puede traer bytes ya disponibles y Quinn
            // no reemite `Readable` para ellos.
            if let Some(reason) = pump_control_streams(slot, self.limits.max_control_frame) {
                slot.fail_close(now, CLOSE_CONTROL_FRAMING, reason);
                return;
            }
            if slot.closed {
                return;
            }
            // `send_control` no tiene un `Instant` para cerrar: si su stream de
            // control sufrió un fallo permanente (no contrapresión), el framing
            // ya no es recuperable y el cierre ocurre aquí, con `now` a mano.
            if slot.tx_failed {
                slot.fail_close(now, CLOSE_CONTROL_FRAMING, SEND_FAILED);
                return;
            }
        }

        let _ = pump_control_streams(slot, self.limits.max_control_frame);
    }
}

impl Slot {
    /// Revoca la autorización y vacía todo lo entregable o reintentable.
    ///
    /// Cierre por violación de protocolo y pérdida remota comparten esta purga:
    /// tras ella no queda identidad, ni capacidades, ni colas RX/TX, ni buffers
    /// de frames a medio llegar que un caller pueda seguir leyendo, y las rutas
    /// públicas caen en `Unauthorized` (`send_*`) o `None` (`recv_*`, getters).
    ///
    /// Es idempotente: llamarla dos veces no cambia el resultado ni deja restos.
    /// No toca `outgoing` (transmisiones ya cifradas, incluido el
    /// CONNECTION_CLOSE del cierre) ni `conn`: el protocolo sigue su curso.
    fn revoke(&mut self) {
        self.authorized = false;
        self.identity = None;
        self.capabilities = Capabilities::none();
        self.audio.clear();
        self.control.clear();
        self.recv_streams.clear();
        self.send_stream = None;
        self.control_rx.clear();
        self.control_tx.clear();
        self.tx_failed = false;
    }

    /// Cierre por violación de protocolo: cierra el QUIC si aún no se cerró y
    /// aplica [`Slot::revoke`].
    fn fail_close(&mut self, now: Instant, code: u32, reason: &'static str) {
        if !self.closed {
            self.conn
                .close(now, VarInt::from_u32(code), Bytes::from_static(b"protocol"));
        }
        self.closed = true;
        self.revoke();
        let _ = reason;
    }

    /// `true` sólo la primera vez: `ServerEvent::Closed` se emite una vez por
    /// sesión aunque el cierre se observe antes de que la conexión drene.
    fn announce_close(&mut self) -> bool {
        if self.close_announced {
            return false;
        }
        self.close_announced = true;
        true
    }
}

/// Atiende los streams de control de un peer y aplica la política de framing.
///
/// Devuelve `Some(reason)` cuando la sesión debe cerrarse (framing corrupto,
/// stream interrumpido o frame truncado). Un mensaje con framing íntegro pero
/// contenido inválido y `request_id` recuperable recibe `error` en el mismo
/// stream y no cierra la conexión: si QUIC no concede crédito (`Blocked`), la
/// respuesta queda encolada con su offset y se reintenta en el siguiente
/// `Writable`/pump, sin cerrar ni duplicar bytes.
fn pump_control_streams(slot: &mut Slot, max_frame: usize) -> Option<&'static str> {
    let streams: Vec<StreamId> = slot.recv_streams.iter().copied().collect();
    // El permiso de encolar es el que exige `recv_control` para entregar. Sin él
    // el frame válido se drena y se descarta: no se retiene y el resto del
    // stream (framing, `Reject`, límites) se atiende igual.
    let accept = slot.authorized && slot.capabilities.control_write;
    for stream in streams {
        match read_control(
            &mut slot.conn,
            stream,
            &mut slot.control_rx,
            &mut slot.control,
            max_frame,
            accept,
        ) {
            ControlRead::Idle => {}
            ControlRead::Eof => {
                // FIN del lado de lectura del peer: el stream sale de lectura,
                // pero una respuesta pendiente del nuestro aún puede escribirse.
                slot.recv_streams.remove(&stream);
                slot.control_rx.remove(stream);
            }
            ControlRead::Framing(reason) => return Some(reason),
            ControlRead::Reject(reply) => {
                let Ok(frame) = wire::encode_message(&reply) else {
                    return Some("respuesta de error no codificable");
                };
                if frame.len() > max_frame {
                    return Some("respuesta de error excede el límite");
                }
                if !slot.control_tx.enqueue(stream, frame) {
                    return Some("demasiadas respuestas de error pendientes");
                }
            }
        }
    }
    // `Writable` no siempre llega por evento si el crédito se liberó en el mismo
    // datagrama; reintentar aquí cubre ambos caminos y sólo escribe lo que quepa.
    if flush_replies(slot).is_err() {
        return Some(REPLY_FAILED);
    }
    None
}

/// Reintenta las respuestas pendientes de `slot`.
///
/// Se saca la cola del `Slot` para poder prestar `conn` en mutable sin aliasing;
/// volver a colocarla conserva exactamente los offsets ya escritos.
fn flush_replies(slot: &mut Slot) -> Result<(), ()> {
    let mut queue = std::mem::take(&mut slot.control_tx);
    let result = queue.flush(&mut slot.conn);
    slot.control_tx = queue;
    result
}

/// Tope de streams con respuesta pendiente: el mismo tope defensivo que usa la
/// lectura (`max_control_streams` acota los streams concurrentes de QUIC).
const MAX_PENDING_REPLY_STREAMS: usize = MAX_CONTROL_BUFFERS;
/// Tope de frames encolados por stream mientras el crédito no llega. Cubre los
/// envíos públicos de `send_control` además de las respuestas `error`.
const MAX_PENDING_REPLY_FRAMES: usize = 8;
/// Tope de bytes pendientes por sesión (8 frames máximos). Superarlo es abuso:
/// el peer abre más respuestas de las que su propia ventana puede aceptar.
const MAX_PENDING_REPLY_BYTES: usize = MAX_CONTROL_FRAME * 8;

/// Frames de control aceptados por QUIC pero aún no escritos del todo.
///
/// Los usan dos caminos con el mismo requisito de orden: las respuestas `error`
/// del servidor/cliente y los envíos públicos de `send_control`. Compartir la
/// cola por `StreamId` es deliberado — dos productores sobre un mismo stream no
/// pueden intercalar prefijos ni reiniciar offsets.
///
/// `Blocked` no es error: `offset` conserva lo ya escrito en el frame en curso y
/// el resto se reintenta en el siguiente `Writable`/pump, así que un `Ok` de la
/// API significa "frame íntegro aceptado para entrega asíncrona". La cola por
/// stream preserva el orden si llegan más frames mientras hay uno pendiente, y
/// los topes (streams, frames, bytes) impiden que un peer que no libera crédito
/// convierta esto en un sumidero sin límite.
#[derive(Debug, Default)]
struct PendingReplies {
    per_stream: HashMap<StreamId, PendingReply>,
}

#[derive(Debug, Default)]
struct PendingReply {
    frames: VecDeque<Vec<u8>>,
    /// Bytes ya escritos del frame en cabeza; el resto nunca se reenvía.
    offset: usize,
}

impl PendingReplies {
    /// Encola `frame` para `stream`. `false` significa tope superado y la sesión
    /// se cierra: descartar una respuesta rompería el contrato y duplicarla no
    /// es una opción.
    fn enqueue(&mut self, stream: StreamId, frame: Vec<u8>) -> bool {
        if !self.per_stream.contains_key(&stream)
            && self.per_stream.len() >= MAX_PENDING_REPLY_STREAMS
        {
            return false;
        }
        if self.total_bytes() + frame.len() > MAX_PENDING_REPLY_BYTES {
            return false;
        }
        let reply = self.per_stream.entry(stream).or_default();
        if reply.frames.len() >= MAX_PENDING_REPLY_FRAMES {
            return false;
        }
        reply.frames.push_back(frame);
        true
    }

    fn total_bytes(&self) -> usize {
        self.per_stream
            .values()
            .flat_map(|reply| reply.frames.iter())
            .map(Vec::len)
            .sum()
    }

    /// Escribe lo que el crédito permita. `Ok` deja encolado lo que no cupo;
    /// `Err` es un fallo permanente del stream (reset, cerrado o desconocido).
    fn flush(&mut self, conn: &mut Connection) -> Result<(), ()> {
        let streams: Vec<StreamId> = self.per_stream.keys().copied().collect();
        for stream in streams {
            let Some(reply) = self.per_stream.get_mut(&stream) else {
                continue;
            };
            loop {
                let written = {
                    let Some(frame) = reply.frames.front() else {
                        break;
                    };
                    let start = reply.offset;
                    let mut send = conn.send_stream(stream);
                    match send.write(&frame[start..]) {
                        // Sin crédito o sin progreso: queda pendiente y se
                        // reintenta al llegar `Writable`. Nunca es un cierre.
                        Ok(0) | Err(WriteError::Blocked) => break,
                        Ok(count) => count,
                        // Reset/Stopped/cerrado: el peer no aceptará esta
                        // respuesta nunca; es un fallo permanente, no `Blocked`.
                        Err(_) => return Err(()),
                    }
                };
                reply.offset += written;
                if let Some(frame) = reply.frames.front() {
                    if reply.offset >= frame.len() {
                        reply.frames.pop_front();
                        reply.offset = 0;
                    }
                }
            }
            if reply.frames.is_empty() {
                self.per_stream.remove(&stream);
            }
        }
        Ok(())
    }

    fn clear(&mut self) {
        self.per_stream.clear();
    }
}

/// Acepta `frame` para entrega asíncrona por `stream`, compartiendo la cola
/// acotada con las respuestas `error`.
///
/// Contrato (el mismo que documenta `SessionServer::send_control`):
/// - `Ok(())`: el frame quedó **íntegro** en cola; lo ya escrito no se reenvía
///   (el `offset` vive en la cola) y el resto sale en el siguiente
///   `Writable`/pump. La contrapresión no es un fallo.
/// - `Err(QueueFull)`: la cola no admite el frame. Se decide **antes** de
///   escribir un byte, así que este frame no deja un prefijo huérfano.
/// - `Err(Stream(..))`: fallo **permanente** del stream al vaciar la cola
///   (reset, stopped, desconocido). No garantiza cero bytes del frame ya
///   aceptado: puede quedar un prefijo escrito. El caller descarta la cola,
///   marca `tx_failed` y cierra la sesión en el siguiente `pump` en vez de
///   reintentar como si el framing fuera recuperable.
fn send_queued(
    queue: &mut PendingReplies,
    conn: &mut Connection,
    stream: StreamId,
    frame: Vec<u8>,
) -> Result<(), TransportError> {
    if !queue.enqueue(stream, frame) {
        return Err(TransportError::QueueFull);
    }
    if queue.flush(conn).is_err() {
        queue.clear();
        return Err(TransportError::Stream("escritura rechazada"));
    }
    Ok(())
}

/// Número máximo de buffers de control simultáneos por peer.
///
/// `max_control_streams` acota los streams concurrentes que QUIC permite; este
/// tope defensivo impide que un bug futuro de esa capa convierta el mapa en un
/// sumidero sin límite. Superarlo es violación de protocolo, no un descarte.
const MAX_CONTROL_BUFFERS: usize = 8;

/// Buffer de prefijos parciales **por stream de control**.
///
/// El framing de control es un flujo de frames `u32 BE length || payload`. Un
/// prefijo a medio llegar pertenece a un `StreamId` concreto: mezclarlo con
/// bytes de otro stream fabricaría frames que el peer nunca envió. Cada entrada
/// está acotada por `max_control_frame` (total, prefijo incluido) y se retira al
/// llegar EOF, reset o cierre.
#[derive(Debug, Default)]
struct ControlBuffers {
    partial: HashMap<StreamId, Vec<u8>>,
}

impl ControlBuffers {
    /// Reserva la entrada de `stream` si el mapa aún admite otra.
    fn open(&mut self, stream: StreamId) -> Option<&mut Vec<u8>> {
        if !self.partial.contains_key(&stream) && self.partial.len() >= MAX_CONTROL_BUFFERS {
            return None;
        }
        Some(self.partial.entry(stream).or_default())
    }

    fn held(&self, stream: StreamId) -> usize {
        self.partial.get(&stream).map_or(0, Vec::len)
    }

    fn remove(&mut self, stream: StreamId) {
        self.partial.remove(&stream);
    }

    fn clear(&mut self) {
        self.partial.clear();
    }
}

/// Desenlace de una lectura de control, ya sin préstamos sobre la conexión.
#[derive(Debug)]
enum ControlRead {
    /// Sin bytes nuevos (incluye `Blocked`, que nunca es error).
    Idle,
    /// El peer cerró su lado (FIN) sin dejar un frame a medias.
    Eof,
    /// Framing corrupto o stream interrumpido: la sesión debe cerrarse.
    Framing(&'static str),
    /// Frame con framing íntegro pero contenido inválido y `request_id`
    /// recuperable: el contrato pide `error` en el mismo stream.
    Reject(ControlMessage),
}

/// ¿Cabe un payload de `payload_len` en un frame de `max_frame` bytes?
///
/// `max_frame` es el frame **total** (prefijo `u32` incluido) y el prefijo del
/// wire declara el **payload** (docs/protocol.md §3: payload `1..=16384`, total
/// `<= 16388`). La comparación correcta es por tanto `payload_len <=
/// max_frame - 4`; comparar contra `max_frame` aceptaría 4 bytes de más y
/// dejaría pasar un frame de 16389 con payload 16385. `SessionLimits::validate`
/// garantiza `max_frame > CONTROL_LENGTH_BYTES`, así que la resta no satura en
/// las rutas reales; `saturating_sub` sólo evita el underflow en una llamada
/// defensiva con un límite degenerado (que rechaza todo prefijo).
fn control_prefix_fits(payload_len: usize, max_frame: usize) -> bool {
    payload_len != 0 && payload_len <= max_frame.saturating_sub(CONTROL_LENGTH_BYTES)
}

/// Lee y decodifica frames de control de `stream`.
///
/// Política:
/// - `Blocked` no es error: se conserva el prefijo y se reintenta en el
///   siguiente `Readable`/pump. El prefijo vive en el buffer de **este** stream.
/// - Prefijo `u32` fuera de `1..=max_frame - 4` (el prefijo declara payload, y
///   `max_frame` es el total con prefijo), exceso sobre el límite, reset del
///   peer y FIN con un frame incompleto son framing corrupto: se cierra la
///   conexión (fail-closed) y no se entrega nada más.
/// - Contenido inválido con framing íntegro sólo se responde con `error` si el
///   `request_id` es recuperable; si no lo es, el contrato no permite inventarlo
///   y también se cierra. Un frame `error` inválido nunca genera otro `error`.
///
/// `accept` decide únicamente el **ingreso en la cola de aplicación**; los bytes
/// se drenan, se acumulan en el buffer del stream y se decodifican siempre, y
/// todos los caminos de framing/`Reject`/límites quedan iguales. Con `accept` en
/// `false` el frame válido se consume y se descarta, así que un receptor sin el
/// permiso correspondiente no retiene payload ajeno. Sin ese drenaje, dejar el
/// stream sin leer movería el residuo al buffer de QUIC y rompería el control de
/// flujo del que dependen los frames siguientes del mismo stream.
fn read_control(
    conn: &mut Connection,
    stream: StreamId,
    buffers: &mut ControlBuffers,
    queue: &mut VecDeque<ControlMessage>,
    max_frame: usize,
    accept: bool,
) -> ControlRead {
    loop {
        let (data, finished, failure) = {
            let mut recv = conn.recv_stream(stream);
            let mut chunks = match recv.read(true) {
                Ok(chunks) => chunks,
                // El stream ya no es legible: no hay nada que conservar.
                Err(_) => return ControlRead::Idle,
            };
            let mut data = Vec::new();
            let mut finished = false;
            let mut failure: Option<&'static str> = None;
            loop {
                match chunks.next(usize::MAX) {
                    Ok(Some(chunk)) => {
                        if buffers.held(stream) + data.len() + chunk.bytes.len() > max_frame {
                            failure = Some("frame de control excede el límite");
                            break;
                        }
                        data.extend_from_slice(&chunk.bytes);
                    }
                    Ok(None) => {
                        finished = true;
                        break;
                    }
                    // `Blocked`: sin más datos por ahora; se conserva el prefijo
                    // y se reintenta al llegar `Readable`.
                    Err(ReadError::Blocked) => break,
                    // Reset del peer: un stream de control interrumpido no deja
                    // un frame utilizable.
                    Err(ReadError::Reset(_)) => {
                        failure = Some("stream de control reseteado por el peer");
                        break;
                    }
                }
            }
            let _ = chunks.finalize();
            (data, finished, failure)
        };
        if let Some(reason) = failure {
            buffers.remove(stream);
            return ControlRead::Framing(reason);
        }
        if !data.is_empty() {
            match buffers.open(stream) {
                Some(buffer) => buffer.extend_from_slice(&data),
                None => return ControlRead::Framing("demasiados streams de control"),
            }
        }

        let mut framing: Option<&'static str> = None;
        let mut reject: Option<ControlMessage> = None;
        let Some(buffer) = buffers.open(stream) else {
            return ControlRead::Framing("demasiados streams de control");
        };
        while buffer.len() >= CONTROL_LENGTH_BYTES {
            let len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
            // El prefijo declara payload, no total: `control_prefix_fits` resta
            // el prefijo del límite total y rechaza `0` o > payload máximo.
            if !control_prefix_fits(len, max_frame) {
                framing = Some("prefijo de longitud fuera de 1..=16384");
                break;
            }
            if buffer.len() < CONTROL_LENGTH_BYTES + len {
                break;
            }
            let end = CONTROL_LENGTH_BYTES + len;
            let frame: Vec<u8> = buffer[..end].to_vec();
            buffer.drain(..end);
            match wire::decode_message(&frame) {
                Ok(message) => {
                    // El frame ya se consumió del stream y se decodificó: el
                    // framing sigue exacto. Sólo el ingreso a la cola de
                    // aplicación depende de `accept`, para no retener payload
                    // de un peer sin el permiso de escritura.
                    if accept {
                        if queue.len() >= MAX_QUEUED_CONTROL {
                            queue.pop_front();
                        }
                        queue.push_back(message);
                    }
                }
                Err(error) => {
                    // El framing quedó íntegro (se consumió exactamente `end`
                    // bytes), así que el resto del stream sigue siendo válido y
                    // se conserva para el siguiente pump.
                    if is_error_frame(&frame) && !error.closes_connection() {
                        // Un `error` inválido no se responde con otro `error`
                        // ni justifica cerrar: se descarta y se sigue leyendo.
                    } else if error.closes_connection() {
                        framing = Some("mensaje de control inválido");
                    } else {
                        reject = error.to_error_message();
                        if reject.is_none() {
                            framing = Some("mensaje de control inválido");
                        }
                    }
                    if framing.is_some() || reject.is_some() {
                        break;
                    }
                }
            }
        }
        if let Some(reason) = framing {
            buffers.remove(stream);
            return ControlRead::Framing(reason);
        }
        if let Some(reply) = reject {
            return ControlRead::Reject(reply);
        }
        if finished {
            // FIN con bytes a medias = frame truncado: fail-closed. Con buffer
            // vacío es el cierre normal del lado de escritura del peer.
            let truncated = buffers.held(stream) > 0;
            buffers.remove(stream);
            return if truncated {
                ControlRead::Framing("frame de control truncado")
            } else {
                ControlRead::Eof
            };
        }
        if data.is_empty() {
            return ControlRead::Idle;
        }
    }
}

/// ¿El frame es un mensaje `error`? Se usa para no responder `error` a `error`.
fn is_error_frame(frame: &[u8]) -> bool {
    let Ok(payload) = wire::decode_frame(frame) else {
        return false;
    };
    let Ok(wire::JsonValue::Object(entries)) = wire::parse_json(payload) else {
        return false;
    };
    entries.iter().any(|(key, value)| {
        key == "type" && matches!(value, wire::JsonValue::String(name) if name == "error")
    })
}

/// Cliente de sesión sans-IO.
pub struct SessionClient {
    endpoint: Endpoint,
    conn: Connection,
    handle: ConnectionHandle,
    limits: SessionLimits,
    events: VecDeque<ClientEvent>,
    outgoing: VecDeque<(SocketAddr, Vec<u8>)>,
    authorized: bool,
    server_pin: Option<SpkiPin>,
    audio: VecDeque<AudioFrame>,
    control: VecDeque<ControlMessage>,
    recv_streams: HashSet<StreamId>,
    send_stream: Option<StreamId>,
    /// Prefijo parcial por stream, igual que en el servidor: nunca se mezclan
    /// bytes de `StreamId` distintos.
    control_rx: ControlBuffers,
    /// Respuestas `error` aceptadas por QUIC y aún no escritas del todo, más
    /// los envíos públicos de `send_control` (misma cola, mismo orden).
    control_tx: PendingReplies,
    /// `send_control` sufrió un fallo permanente del stream de control: el
    /// siguiente `pump` cierra la sesión.
    tx_failed: bool,
    /// `ClientEvent::Closed` ya entregado: el cierre se anuncia una sola vez.
    close_announced: bool,
    scratch: Vec<u8>,
}

impl SessionClient {
    /// Inicia una conexión de sesión contra `server_addr`.
    ///
    /// `server_pin` es el pin SPKI de la identidad persistente del daemon. El
    /// handshake no envía datos de aplicación hasta validar cadena, validez,
    /// ALPN y pin.
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        identity: Arc<CertifiedKey>,
        server_roots: Arc<RootCertStore>,
        server_pin: SpkiPin,
        limits: SessionLimits,
        now: Instant,
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<Self, TransportError> {
        Self::connect_inner(
            Some(identity),
            server_roots,
            server_pin,
            limits,
            now,
            server_addr,
            server_name,
        )
    }

    /// Variante sin certificado de cliente, sólo para probar el rechazo del
    /// servidor. El servidor de sesión exige mTLS: este cliente nunca obtiene
    /// una sesión autorizada.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn connect_without_client_identity(
        server_roots: Arc<RootCertStore>,
        server_pin: SpkiPin,
        limits: SessionLimits,
        now: Instant,
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<Self, TransportError> {
        Self::connect_inner(
            None,
            server_roots,
            server_pin,
            limits,
            now,
            server_addr,
            server_name,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn connect_inner(
        identity: Option<Arc<CertifiedKey>>,
        server_roots: Arc<RootCertStore>,
        server_pin: SpkiPin,
        limits: SessionLimits,
        now: Instant,
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<Self, TransportError> {
        limits.validate()?;
        if let Some(identity) = identity.as_ref() {
            if identity.cert.len() > MAX_CERT_CHAIN {
                return Err(TransportError::Identity(IdentityError::ChainTooLong));
            }
        }
        let verifier = PinnedServerVerifier::new(server_roots, server_pin)?;
        let builder = TlsClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier));
        let mut tls = match identity {
            Some(identity) => {
                builder.with_client_cert_resolver(Arc::new(StaticClientCertResolver::new(identity)))
            }
            None => builder.with_no_client_auth(),
        };
        tls.alpn_protocols = vec![ALPN_SESSION.to_vec()];
        tls.enable_early_data = false;
        let crypto = QuicClientConfig::try_from(tls)
            .map_err(|error| TransportError::Tls(error.to_string()))?;
        let mut quinn = QuinnClientConfig::new(Arc::new(crypto));
        quinn.transport_config(transport_config(&limits, true)?);
        let mut endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None);
        let (handle, conn) = endpoint
            .connect(now, quinn, server_addr, server_name)
            .map_err(|error| TransportError::Connect(error.to_string()))?;
        Ok(Self {
            endpoint,
            conn,
            handle,
            limits,
            events: VecDeque::new(),
            outgoing: VecDeque::new(),
            authorized: false,
            server_pin: None,
            audio: VecDeque::new(),
            control: VecDeque::new(),
            recv_streams: HashSet::new(),
            send_stream: None,
            control_rx: ControlBuffers::default(),
            control_tx: PendingReplies::default(),
            tx_failed: false,
            close_announced: false,
            scratch: Vec::new(),
        })
    }

    /// Procesa un datagrama del servidor.
    pub fn handle_datagram(
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
        self.pump(now)
    }

    /// Siguiente evento del cliente.
    pub fn next_event(&mut self) -> Option<ClientEvent> {
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

    /// Próximo timeout.
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        self.conn.poll_timeout()
    }

    /// Despierta timeouts vencidos.
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), TransportError> {
        self.conn.handle_timeout(now);
        self.pump(now)
    }

    /// ¿Sesión autorizada?
    pub fn is_authorized(&self) -> bool {
        self.authorized
    }

    /// `true` si el handshake ofreció 0-RTT (nunca habilitado por este crate).
    pub fn has_0rtt(&self) -> bool {
        self.conn.has_0rtt()
    }

    /// `true` si el peer aceptó datos 0-RTT (nunca esperado).
    pub fn accepted_0rtt(&self) -> bool {
        self.conn.accepted_0rtt()
    }

    /// Pin del servidor verificado, si la sesión está autorizada.
    pub fn server_pin(&self) -> Option<SpkiPin> {
        self.server_pin
    }

    /// Envía un frame de audio (requiere `mic` concedido por el servidor; este
    /// extremo no puede saberlo, el servidor rechaza si falta).
    pub fn send_audio(&mut self, frame: &AudioFrame) -> Result<(), TransportError> {
        if !self.authorized {
            return Err(TransportError::Unauthorized);
        }
        let wire = frame.encode();
        if wire.len() > self.limits.max_audio_datagram {
            return Err(TransportError::Audio(FrameError::Oversized {
                len: wire.len(),
            }));
        }
        self.conn
            .datagrams()
            .send(Bytes::copy_from_slice(&wire), true)
            .map_err(|_| TransportError::Datagram("audio"))
    }

    /// Recibe audio de retorno (si el servidor lo concede).
    pub fn recv_audio(&mut self) -> Option<AudioFrame> {
        self.audio.pop_front()
    }

    /// Envía un mensaje de control por el stream bidireccional de control.
    ///
    /// Mismo contrato que [`SessionServer::send_control`]: `Ok(())` es
    /// aceptación del frame **íntegro** para entrega asíncrona (cola acotada
    /// con offset, reintentada en `Writable`/pump), nunca entrega síncrona ni
    /// garantía de recepción. `Err(QueueFull)` y los rechazos de validación,
    /// autorización o apertura del stream se deciden antes de aceptar el frame,
    /// sin bytes nuevos de él. `Err(Stream(..))` por un fallo permanente al
    /// vaciar la cola **no** garantiza cero bytes: puede haber un prefijo
    /// escrito; la cola se descarta, `tx_failed` queda marcado y la sesión se
    /// cierra en el siguiente `pump`, sin reintentar en ella como si estuviera
    /// limpia.
    pub fn send_control(&mut self, message: &ControlMessage) -> Result<(), TransportError> {
        if !self.authorized {
            return Err(TransportError::Unauthorized);
        }
        let frame = wire::encode_message(message).map_err(TransportError::Control)?;
        if frame.len() > self.limits.max_control_frame {
            return Err(TransportError::Control(WireError::invalid(None)));
        }
        let stream = match self.send_stream {
            Some(stream) => stream,
            None => {
                let stream = self
                    .conn
                    .streams()
                    .open(Dir::Bi)
                    .ok_or(TransportError::Stream("sin stream de control"))?;
                self.send_stream = Some(stream);
                stream
            }
        };
        match send_queued(&mut self.control_tx, &mut self.conn, stream, frame) {
            Ok(()) => Ok(()),
            Err(error) => {
                if matches!(error, TransportError::Stream(_)) {
                    self.tx_failed = true;
                }
                Err(error)
            }
        }
    }

    /// Recibe el siguiente mensaje de control del servidor.
    pub fn recv_control(&mut self) -> Option<ControlMessage> {
        self.control.pop_front()
    }

    fn pump(&mut self, now: Instant) -> Result<(), TransportError> {
        while let Some(event) = self.conn.poll_endpoint_events() {
            if let Some(event) = self.endpoint.handle_event(self.handle, event) {
                self.conn.handle_event(event);
            }
        }
        while let Some(event) = self.conn.poll() {
            match event {
                Event::Connected => {
                    if self.authorized {
                        continue;
                    }
                    let handshake = self
                        .conn
                        .crypto_session()
                        .handshake_data()
                        .and_then(|data| data.downcast::<HandshakeData>().ok());
                    let Some(handshake) = handshake else {
                        self.events
                            .push_back(ClientEvent::Rejected(RejectReason::Tls));
                        return Ok(());
                    };
                    if handshake.protocol.as_deref() != Some(ALPN_SESSION) {
                        self.events
                            .push_back(ClientEvent::Rejected(RejectReason::Alpn));
                        return Ok(());
                    }
                    let peer = self
                        .conn
                        .crypto_session()
                        .peer_identity()
                        .and_then(|data| data.downcast::<Vec<CertificateDer<'static>>>().ok());
                    let Some(chain) = peer else {
                        self.events
                            .push_back(ClientEvent::Rejected(RejectReason::NoPeerCertificate));
                        return Ok(());
                    };
                    let Some(leaf) = chain.first() else {
                        self.events
                            .push_back(ClientEvent::Rejected(RejectReason::NoPeerCertificate));
                        return Ok(());
                    };
                    let pin = match SpkiPin::from_certificate_der(leaf.as_ref()) {
                        Ok(pin) => pin,
                        Err(_) => {
                            self.events
                                .push_back(ClientEvent::Rejected(RejectReason::MalformedIdentity));
                            return Ok(());
                        }
                    };
                    self.authorized = true;
                    self.server_pin = Some(pin);
                    self.events
                        .push_back(ClientEvent::Authorized { server: pin });
                }
                Event::ConnectionLost { .. } => {
                    // Cierre: misma purga que un fallo de protocolo, incluida la
                    // cola TX (`control_tx`) que antes sobrevivía al cierre.
                    self.drop_session("connection lost");
                    return Ok(());
                }
                Event::DatagramReceived => {
                    if !self.authorized {
                        continue;
                    }
                    while let Some(datagram) = self.conn.datagrams().recv() {
                        if datagram.len() > self.limits.max_audio_datagram {
                            self.conn.close(
                                now,
                                VarInt::from_u32(CLOSE_AUDIO_OVERSIZED),
                                Bytes::from_static(b"oversized"),
                            );
                            return Ok(());
                        }
                        if let Ok(frame) = AudioFrame::decode(&datagram) {
                            if self.audio.len() >= MAX_QUEUED_AUDIO {
                                self.audio.pop_front();
                            }
                            self.audio.push_back(frame);
                        }
                    }
                }
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                    if !self.authorized {
                        continue;
                    }
                    while let Some(stream) = self.conn.streams().accept(Dir::Bi) {
                        self.recv_streams.insert(stream);
                    }
                }
                Event::Stream(StreamEvent::Readable { id: stream }) => {
                    if !self.authorized {
                        continue;
                    }
                    self.recv_streams.insert(stream);
                }
                Event::Stream(StreamEvent::Writable { .. }) => {
                    // Crédito liberado: reanudar la respuesta pendiente.
                    if self.control_tx.flush(&mut self.conn).is_err() {
                        self.conn.close(
                            now,
                            VarInt::from_u32(CLOSE_CONTROL_FRAMING),
                            Bytes::from_static(b"protocol"),
                        );
                        self.drop_session(REPLY_FAILED);
                        return Ok(());
                    }
                }
                Event::Stream(StreamEvent::Finished { .. })
                | Event::Stream(StreamEvent::Stopped { .. }) => {
                    // Eventos del medio de envío; el fin del lado de lectura se
                    // observa como `Ok(None)` al leer.
                }
                _ => {}
            }
            if let Some(reason) = self.pump_control(self.limits.max_control_frame) {
                self.conn.close(
                    now,
                    VarInt::from_u32(CLOSE_CONTROL_FRAMING),
                    Bytes::from_static(b"protocol"),
                );
                self.drop_session(reason);
                return Ok(());
            }
            // Un fallo permanente del stream de control en `send_control` deja
            // el framing irrecuperable: se cierra aquí, con `now` a mano.
            if self.tx_failed {
                self.conn.close(
                    now,
                    VarInt::from_u32(CLOSE_CONTROL_FRAMING),
                    Bytes::from_static(b"protocol"),
                );
                self.drop_session(SEND_FAILED);
                return Ok(());
            }
        }
        if let Some(reason) = self.pump_control(self.limits.max_control_frame) {
            self.conn.close(
                now,
                VarInt::from_u32(CLOSE_CONTROL_FRAMING),
                Bytes::from_static(b"protocol"),
            );
            self.drop_session(reason);
        }
        Ok(())
    }
}

impl SessionClient {
    /// Revoca la sesión y borra todo estado entregable tras un cierre.
    ///
    /// Idempotente: `ClientEvent::Closed` se encola una sola vez por sesión
    /// aunque el cierre se observe por varias rutas (fallo de protocolo y
    /// `ConnectionLost` posterior).
    fn drop_session(&mut self, _reason: &'static str) {
        self.authorized = false;
        self.server_pin = None;
        self.audio.clear();
        self.control.clear();
        self.control_rx.clear();
        self.control_tx.clear();
        self.tx_failed = false;
        self.outgoing.clear();
        self.recv_streams.clear();
        self.send_stream = None;
        if !self.close_announced {
            self.close_announced = true;
            self.events.push_back(ClientEvent::Closed);
        }
    }

    /// Atiende los streams de control del servidor. Igual que en el servidor,
    /// un frame inválido con framing íntegro y `request_id` recuperable recibe
    /// `error` en el mismo stream; el framing corrupto cierra la conexión.
    fn pump_control(&mut self, max_frame: usize) -> Option<&'static str> {
        let streams: Vec<StreamId> = self.recv_streams.iter().copied().collect();
        for stream in streams {
            match read_control(
                &mut self.conn,
                stream,
                &mut self.control_rx,
                &mut self.control,
                max_frame,
                // El cliente no dispone de la ACL remota: mantiene el
                // comportamiento autorizado actual, sin aplicar el permiso del
                // servidor.
                true,
            ) {
                ControlRead::Idle => {}
                ControlRead::Eof => {
                    self.recv_streams.remove(&stream);
                    self.control_rx.remove(stream);
                }
                ControlRead::Framing(reason) => return Some(reason),
                ControlRead::Reject(reply) => {
                    let Ok(frame) = wire::encode_message(&reply) else {
                        return Some("respuesta de error no codificable");
                    };
                    if frame.len() > max_frame {
                        return Some("respuesta de error excede el límite");
                    }
                    if !self.control_tx.enqueue(stream, frame) {
                        return Some("demasiadas respuestas de error pendientes");
                    }
                }
            }
        }
        // Igual que en el servidor: reintentar tras cada pump cubre el caso en
        // que el crédito llegó antes que el `Writable`, y sólo escribe lo que quepa.
        self.control_tx
            .flush(&mut self.conn)
            .is_err()
            .then_some(REPLY_FAILED)
    }
}

/// Reexporta `certified_key` para consumidores que construyen su identidad.
pub use crate::identity::certified_key as identity_certified_key;

#[cfg(test)]
#[path = "quinn_regressions.rs"]
mod quinn_regressions;

#[cfg(test)]
mod control_frame_limits {
    //! Fronteras del límite de frame de control: unidades **payload** vs
    //! **total**. El wire declara payload en el prefijo `u32`; el límite
    //! configurado es total. Este módulo fija esa conversión sin tocar la API
    //! pública ni los tests externos del crate.

    use super::{control_prefix_fits, SessionLimits, MAX_CONTROL_FRAME};
    use arl_proto::wire::CONTROL_LENGTH_BYTES;

    /// `MAX_CONTROL_FRAME` es total: payload máximo + prefijo.
    #[test]
    fn max_control_frame_is_total_including_prefix() {
        assert_eq!(MAX_CONTROL_FRAME, 16_388, "total documentado en §3");
        assert_eq!(
            MAX_CONTROL_FRAME - CONTROL_LENGTH_BYTES,
            16_384,
            "payload máximo documentado en §3"
        );
        assert_eq!(
            SessionLimits::default().max_control_frame,
            MAX_CONTROL_FRAME
        );
    }

    /// Frontera payload 16384/16385 con el límite por defecto.
    #[test]
    fn payload_boundary_16384_ok_16385_rejected() {
        let total = SessionLimits::default().max_control_frame;
        assert!(
            control_prefix_fits(16_384, total),
            "payload 16384 cabe: total 16388 == límite"
        );
        assert!(
            !control_prefix_fits(16_385, total),
            "payload 16385 no cabe: total 16389 > 16388"
        );
        assert!(
            !control_prefix_fits(0, total),
            "prefijo 0 es framing corrupto"
        );
        assert!(control_prefix_fits(1, total), "payload mínimo 1");
    }

    /// Un límite configurado menor se interpreta como total y resta el prefijo.
    #[test]
    fn configured_limit_is_total_not_payload() {
        assert!(control_prefix_fits(1_020, 1_024));
        assert!(!control_prefix_fits(1_021, 1_024));
        assert!(
            !control_prefix_fits(1_024, 1_024),
            "un límite de 1024 no admite payload 1024 (serían 1028 en el wire)"
        );
    }

    /// La validación rechaza límites que no dejan payload, y acepta el máximo.
    #[test]
    fn validate_rejects_limits_without_payload_room() {
        let mut limits = SessionLimits {
            max_control_frame: CONTROL_LENGTH_BYTES,
            ..Default::default()
        };
        assert!(
            limits.validate().is_err(),
            "sin bytes de payload no es válido"
        );

        limits.max_control_frame = CONTROL_LENGTH_BYTES + 1;
        assert!(limits.validate().is_ok(), "payload 1 es el mínimo válido");

        limits.max_control_frame = MAX_CONTROL_FRAME + 1;
        assert!(
            limits.validate().is_err(),
            "por encima del total máximo no es válido"
        );

        limits.max_control_frame = MAX_CONTROL_FRAME;
        assert!(limits.validate().is_ok(), "el total máximo es válido");
    }
}

#[cfg(test)]
#[path = "quinn_security_regressions.rs"]
mod quinn_security_regressions;
