//! Proceso de producto: un `SessionServer` y un `SharedControl` en el mismo
//! proceso que el socket Unix del CLI.
//!
//! El admin local entra por `local_line`. El remoto entra por el UDP de la
//! sesión ya autorizada. Los dos ven la misma revisión porque comparten el
//! mismo `SharedControl`. No hay un segundo protocolo ni un puerto en claro.
//!
//! La identidad, la CA de clientes y las identidades aprobadas se leen de
//! archivos que el operador aporta. Este módulo no genera claves ni
//! certificados. Sin esos archivos el proceso arranca sólo con el admin local
//! y deja el endpoint QUIC apagado: ese es el hueco de producción, no se
//! rellena con una CA fabricada.

use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arl_audio_linux::{ring, SampleProducer};
use arl_proto::control::{ErrorCode, PeerRole, RemoteAction};
use arl_proto::wire::{decode_frame, encode_message, ControlMessage};
use arl_transport::identity::{self, SpkiPin};
use arl_transport::quinn::{
    Acl, ServerEvent, SessionId, SessionLimits, SessionServer, TransportError,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde_json::{json, Value};

use crate::control::{ControlError, ControlRegistry, IdentityGrant, SharedControl};
use crate::pairing;
use crate::session::{SessionAudio, SessionAudioStats};

/// Tope de una línea de control que llega por el socket Unix.
const MAX_LINE: usize = 64 * 1024;
/// Cuánto espera el hilo de QUIC un datagrama antes de volver a mirar el paro.
const READ_TICK: Duration = Duration::from_millis(20);
/// Muestras del anillo local. Nadie lo consume: el micrófono no se abre.
const RING_SAMPLES: usize = 480;

/// Archivos que el operador aporta para abrir el endpoint de sesión.
///
/// Ninguno se fabrica aquí. `identity_der` es el certificado del servidor,
/// `identity_key` su PKCS#8 y `client_ca_der` la CA que firma a los clientes.
/// `identities` es el archivo que ya escribe el emparejamiento.
#[derive(Debug, Clone)]
pub struct ProvisionFiles {
    /// Certificado DER del servidor de sesión.
    pub identity_der: PathBuf,
    /// Clave PKCS#8 DER que corresponde a ese certificado.
    pub identity_key: PathBuf,
    /// CA DER que firma los certificados de cliente.
    pub client_ca_der: PathBuf,
    /// Identidades aprobadas (`pairing::load`).
    pub identities: PathBuf,
    /// Permisos de control por pin, uno por línea `pin_hex<TAB>device_id<TAB>read|write`.
    ///
    /// La ACL de transporte no se puede recorrer y no guarda `device_id`.
    /// Este archivo es la lista que el operador ya aprobó para el control.
    /// Un pin que no esté en la ACL cargada no se registra.
    pub control_grants: PathBuf,
}

/// Cómo arranca el proceso.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Dónde escucha QUIC. `None` deja el endpoint apagado.
    pub quic_bind: Option<SocketAddr>,
    /// Archivos de identidad. Obligatorios si hay `quic_bind`.
    pub provision: Option<ProvisionFiles>,
    /// `device_id` del admin local (8..=64, `[A-Za-z0-9_-]`).
    pub local_device_id: String,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            quic_bind: None,
            provision: None,
            local_device_id: "arl-cli-local".to_owned(),
        }
    }
}

/// Lo que el proceso dejó listo antes de entrar al bucle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeReady {
    /// Pin SPKI del certificado de servidor, si QUIC arrancó.
    pub server_pin: Option<String>,
    /// Dirección UDP real (el puerto 0 se resuelve al asignado).
    pub quic_addr: Option<SocketAddr>,
    /// Por qué QUIC no arrancó. `None` si arrancó.
    pub quic_gap: Option<String>,
}

/// Error de arranque. Distinto de un fallo de una petición concreta.
#[derive(Debug)]
pub enum RuntimeError {
    /// Los archivos de provisión no sirven para abrir la sesión.
    Provision(String),
    /// El estado compartido no se pudo abrir.
    Control(ControlError),
    /// El socket UDP no se pudo abrir.
    Socket(io::Error),
    /// El transporte rechazó la configuración.
    Transport(TransportError),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provision(detail) => write!(f, "provisión: {detail}"),
            Self::Control(error) => write!(f, "control: {error}"),
            Self::Socket(error) => write!(f, "socket UDP: {error}"),
            Self::Transport(error) => write!(f, "transporte: {error}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

struct QuicEndpoint {
    socket: UdpSocket,
    server: SessionServer,
    producer: SampleProducer,
    /// Estado de decodificación por sesión (PCM16 u Opus negociado).
    audio: HashMap<SessionId, SessionAudio>,
    /// Sesiones que este proceso vio autorizadas y aún no cerró.
    live: Vec<SessionId>,
}

/// Proceso dueño del estado. El socket Unix y el UDP de QUIC los atiende el
/// mismo hilo: el estado de control no es `Send` y no se parte en dos.
pub struct ProductRuntime {
    control: SharedControl,
    quic: Option<QuicEndpoint>,
    quic_addr: Option<SocketAddr>,
    sessions: u64,
    started: Instant,
}

/// Arranca el estado compartido y, si la provisión está completa, el QUIC.
///
/// Sin archivos de identidad el admin local queda abierto y QUIC apagado.
pub fn start(config: &RuntimeConfig) -> Result<(ProductRuntime, RuntimeReady), RuntimeError> {
    let mut registry = ControlRegistry::new();
    let loaded = match (&config.quic_bind, &config.provision) {
        (Some(addr), Some(files)) => Some(load_quic(*addr, files, &mut registry)?),
        (Some(_), None) => {
            return Err(RuntimeError::Provision(
                "falta la provisión (identidad, CA, identidades y grants)".into(),
            ))
        }
        (None, _) => None,
    };

    let mut control = SharedControl::new(registry);
    control
        .open_local(IdentityGrant {
            device_id: config.local_device_id.clone(),
            role: PeerRole::LinuxCli,
            actions: vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
        })
        .map_err(RuntimeError::Control)?;

    let (quic, ready) = match loaded {
        Some(loaded) => (
            Some(loaded.endpoint),
            RuntimeReady {
                server_pin: Some(loaded.pin),
                quic_addr: Some(loaded.addr),
                quic_gap: None,
            },
        ),
        None => (
            None,
            RuntimeReady {
                server_pin: None,
                quic_addr: None,
                quic_gap: Some(
                    "endpoint QUIC apagado: no hay identidad, CA ni pin aprovisionados".into(),
                ),
            },
        ),
    };

    Ok((
        ProductRuntime {
            control,
            quic_addr: match quic.as_ref() {
                Some(endpoint) => Some(endpoint.socket.local_addr().map_err(|error| {
                    RuntimeError::Provision(format!("dirección local: {error}"))
                })?),
                None => None,
            },
            quic,
            sessions: 0,
            started: Instant::now(),
        },
        ready,
    ))
}

struct LoadedQuic {
    endpoint: QuicEndpoint,
    pin: String,
    addr: SocketAddr,
}

fn load_quic(
    addr: SocketAddr,
    files: &ProvisionFiles,
    registry: &mut ControlRegistry,
) -> Result<LoadedQuic, RuntimeError> {
    let cert = read_cert(&files.identity_der)?;
    let pin = SpkiPin::from_certificate_der(cert.as_ref())
        .map_err(|error| RuntimeError::Provision(format!("pin del servidor: {error}")))?;
    let identity = load_identity(cert, &files.identity_key)?;
    let roots = identity::root_store([read_cert(&files.client_ca_der)?])
        .map_err(|error| RuntimeError::Provision(format!("CA de clientes: {error}")))?;
    let acl = pairing::load(&files.identities).map_err(|error| {
        RuntimeError::Provision(format!(
            "identidades ({}): {error}",
            files.identities.display()
        ))
    })?;
    register_grants(registry, &acl, &files.control_grants)?;
    let server = SessionServer::new(Arc::new(identity), roots, acl, SessionLimits::default())
        .map_err(RuntimeError::Transport)?;
    let socket = UdpSocket::bind(addr).map_err(RuntimeError::Socket)?;
    let bound = socket.local_addr().map_err(RuntimeError::Socket)?;
    let (producer, _consumer) = ring(RING_SAMPLES);
    Ok(LoadedQuic {
        endpoint: QuicEndpoint {
            socket,
            server,
            producer,
            audio: HashMap::new(),
            live: Vec::new(),
        },
        pin: pin.to_hex(),
        addr: bound,
    })
}

/// Registra control sólo para pines que la ACL ya autorizó.
///
/// Una línea es `pin_hex<TAB>device_id<TAB>read` o `write` (write incluye
/// read). El bit de micrófono de la ACL no se copia: no hay acción de
/// micrófono en el control.
fn register_grants(
    registry: &mut ControlRegistry,
    acl: &Acl,
    path: &Path,
) -> Result<(), RuntimeError> {
    let text = fs::read_to_string(path).map_err(|error| {
        RuntimeError::Provision(format!("grants ({}): {error}", path.display()))
    })?;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        let (pin_hex, device_id, mode) = match (parts.next(), parts.next(), parts.next()) {
            (Some(pin), Some(device), Some(mode)) if parts.next().is_none() => (pin, device, mode),
            _ => {
                return Err(RuntimeError::Provision(format!(
                    "grants línea {}: se esperan pin, device_id y read|write",
                    index + 1
                )))
            }
        };
        let pin = SpkiPin::from_hex(pin_hex).ok_or_else(|| {
            RuntimeError::Provision(format!("grants línea {}: pin inválido", index + 1))
        })?;
        let caps = acl.capabilities(&pin).ok_or_else(|| {
            RuntimeError::Provision(format!(
                "grants línea {}: el pin no está en la ACL cargada",
                index + 1
            ))
        })?;
        let write = match mode {
            "read" => false,
            "write" => true,
            _ => {
                return Err(RuntimeError::Provision(format!(
                    "grants línea {}: modo {mode} no es read ni write",
                    index + 1
                )))
            }
        };
        if write && !caps.control_write() {
            return Err(RuntimeError::Provision(format!(
                "grants línea {}: la ACL no concede control_write",
                index + 1
            )));
        }
        if !caps.control_read() {
            return Err(RuntimeError::Provision(format!(
                "grants línea {}: la ACL no concede control_read",
                index + 1
            )));
        }
        let mut actions = vec![RemoteAction::ConfigRead];
        if write {
            actions.push(RemoteAction::ConfigWrite);
        }
        registry
            .register(
                pin,
                IdentityGrant {
                    device_id: device_id.to_owned(),
                    role: PeerRole::Android,
                    actions,
                },
            )
            .map_err(RuntimeError::Control)?;
    }
    Ok(())
}

fn load_identity(
    cert: CertificateDer<'static>,
    key_path: &Path,
) -> Result<rustls::sign::CertifiedKey, RuntimeError> {
    let key_bytes = fs::read(key_path).map_err(|error| {
        RuntimeError::Provision(format!("clave ({}): {error}", key_path.display()))
    })?;
    let key_der = PrivatePkcs8KeyDer::from(key_bytes);
    let signing = any_ecdsa_type(&key_der.into())
        .map_err(|_| RuntimeError::Provision("la clave PKCS#8 no es ECDSA usable".into()))?;
    // `certified_key` rechaza una clave que no corresponde al certificado.
    identity::certified_key(vec![cert], signing)
        .map_err(|error| RuntimeError::Provision(format!("identidad: {error}")))
}

fn read_cert(path: &Path) -> Result<CertificateDer<'static>, RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        RuntimeError::Provision(format!("certificado ({}): {error}", path.display()))
    })?;
    Ok(CertificateDer::from(bytes))
}

impl ProductRuntime {
    /// Segundos desde el arranque y sesiones QUIC autorizadas en esta ejecución.
    pub fn status(&self) -> (u64, u64) {
        (self.started.elapsed().as_secs(), self.sessions)
    }

    /// Revisión que ven el admin local y el remoto.
    pub fn revision(&self) -> u64 {
        self.control.revision()
    }

    /// ¿Hay endpoint QUIC en este proceso?
    pub fn quic_enabled(&self) -> bool {
        self.quic.is_some()
    }

    /// Aplica una línea del socket Unix. El JSON es el que ya habla `arl`.
    pub fn local_line(&mut self, line: &str) -> Value {
        if line.len() > MAX_LINE {
            return error_reply(
                Value::Null,
                ErrorCode::TooLarge,
                "petición demasiado grande",
            );
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return error_reply(Value::Null, ErrorCode::Invalid, "JSON inválido"),
        };
        let id = match req.get("request_id") {
            Some(value) if value.is_string() => value.clone(),
            _ => return error_reply(Value::Null, ErrorCode::Invalid, "falta request_id"),
        };
        let op = match req.get("op").and_then(Value::as_str) {
            Some(op) => op,
            None => return error_reply(id, ErrorCode::Invalid, "falta op"),
        };
        let args = req.get("args").cloned().unwrap_or(Value::Null);
        let id_text = id.as_str().unwrap_or_default();
        match self.dispatch(op, id_text, &args) {
            Ok(result) => json!({"request_id": id, "ok": true, "result": result}),
            Err(error) => json!({"request_id": id, "ok": false, "error": error}),
        }
    }

    fn dispatch(&mut self, op: &str, request_id: &str, args: &Value) -> Result<Value, Value> {
        self.local_op(op, request_id, args)
    }

    /// Una operación ya separada por el socket Unix (`op`, `request_id`, `args`).
    ///
    /// Es la misma firma que usa el CLI hoy. La respuesta `Ok` es el resultado;
    /// `Err` es el objeto `error` del catálogo.
    pub fn local_op(&mut self, op: &str, request_id: &str, args: &Value) -> Result<Value, Value> {
        match op {
            "status" => {
                let (uptime_s, sessions) = self.status();
                Ok(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "uptime_s": uptime_s,
                    "sessions": sessions,
                    "quic": self.quic_addr.map(|addr| addr.to_string()),
                }))
            }
            "config.get" | "config.set" | "capabilities" => self.control_op(op, request_id, args),
            other => Err(failure(
                ErrorCode::Unsupported,
                &format!("op desconocida: {other}"),
            )),
        }
    }

    fn control_op(&mut self, op: &str, request_id: &str, args: &Value) -> Result<Value, Value> {
        let message = local_message(op, request_id, args)?;
        if encode_message(&message).is_err() {
            return Err(failure(ErrorCode::Invalid, "petición de control inválida"));
        }
        let outcome = self
            .control
            .local_call(&message)
            .map_err(|_| failure(ErrorCode::Internal, "admin local no disponible"))?;
        let response = outcome
            .response
            .ok_or_else(|| failure(ErrorCode::Internal, "sin respuesta de control"))?;
        let envelope =
            message_json(&response).map_err(|text| failure(ErrorCode::Internal, &text))?;
        match response {
            ControlMessage::Error(_) => Err(envelope["payload"].clone()),
            _ => Ok(envelope),
        }
    }

    /// Una vuelta del endpoint QUIC: lee, autoriza, responde control y drena audio.
    ///
    /// El audio se decodifica en este hilo, el worker del daemon, y desde aquí
    /// se escribe el anillo que consume PipeWire. La reducción de ruido
    /// opcional, si la sesión se construyó con `SessionAudio::with_ns`, corre en
    /// esta misma vuelta antes del anillo: el callback de tiempo real nunca
    /// ejecuta DSP.
    pub fn pump_quic(&mut self) -> Result<(), RuntimeError> {
        let Some(quic) = self.quic.as_mut() else {
            std::thread::sleep(READ_TICK);
            return Ok(());
        };
        let _ = quic.socket.set_read_timeout(Some(READ_TICK));
        let mut buf = [0u8; 2048];
        match quic.socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let _ = quic
                    .server
                    .handle_datagram(Instant::now(), from, &buf[..len]);
            }
            Err(error) if is_timeout(&error) => {}
            Err(error) => return Err(RuntimeError::Socket(error)),
        }
        let now = Instant::now();
        quic.server
            .handle_timeout(now)
            .map_err(RuntimeError::Transport)?;
        flush_transmit(&quic.socket, &mut quic.server)?;

        while let Some(event) = quic.server.next_event() {
            match event {
                ServerEvent::Authorized { session, .. } => {
                    self.sessions += 1;
                    if !quic.live.contains(&session) {
                        quic.live.push(session);
                    }
                }
                ServerEvent::Closed { session } => {
                    quic.audio.remove(&session);
                    quic.live.retain(|id| *id != session);
                }
                ServerEvent::Rejected { .. } => {}
            }
        }

        let live = quic.live.clone();
        self.control
            .poll(&mut quic.server, &live)
            .map_err(RuntimeError::Control)?;
        flush_transmit(&quic.socket, &mut quic.server)?;
        for session in live {
            let negotiated = match quic.server.identity(session) {
                Some(pin) => self.control.opus_negotiated(&pin),
                None => false,
            };
            let QuicEndpoint {
                server,
                producer,
                audio,
                ..
            } = quic;
            let state = audio
                .entry(session)
                .or_insert_with(|| SessionAudio::new(negotiated));
            if state.set_opus_negotiated(negotiated).is_err() {
                continue;
            }
            while let Some(payload) = server.recv_audio_payload(session) {
                if let Ok(frames) = state.handle(payload) {
                    for frame in frames {
                        let _ = producer.push_slice(&frame.samples);
                    }
                }
            }
        }
        Ok(())
    }

    /// Contadores de audio de una sesión viva (`None` si no existe).
    pub fn audio_stats(&self, session: SessionId) -> Option<SessionAudioStats> {
        self.quic
            .as_ref()?
            .audio
            .get(&session)
            .map(SessionAudio::stats)
    }
}

fn flush_transmit(socket: &UdpSocket, server: &mut SessionServer) -> Result<(), RuntimeError> {
    let now = Instant::now();
    while let Some((to, bytes)) = server.poll_transmit(now) {
        socket.send_to(&bytes, to).map_err(RuntimeError::Socket)?;
    }
    Ok(())
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn local_message(op: &str, request_id: &str, args: &Value) -> Result<ControlMessage, Value> {
    match op {
        "config.get" => {
            let key = match args.get("key") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) => Some(
                    arl_proto::control::ConfigKey::new(name)
                        .map_err(|code| failure(code, "clave de configuración inválida"))?,
                ),
                Some(_) => return Err(failure(ErrorCode::Invalid, "key debe ser texto")),
            };
            Ok(ControlMessage::ConfigGet(arl_proto::wire::ConfigGetMsg {
                request_id: request_id.to_owned(),
                key,
            }))
        }
        "config.set" => {
            let key = match args.get("key").and_then(Value::as_str) {
                Some(name) => arl_proto::control::ConfigKey::new(name)
                    .map_err(|code| failure(code, "clave de configuración inválida"))?,
                None => return Err(failure(ErrorCode::Invalid, "falta key")),
            };
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| failure(ErrorCode::Invalid, "falta expected_revision"))?;
            let value = match args.get("value") {
                Some(Value::Bool(flag)) => arl_proto::control::ConfigValue::Bool(*flag),
                Some(Value::String(text)) => arl_proto::control::ConfigValue::String(text.clone()),
                Some(Value::Number(number)) => match (number.as_i64(), number.as_f64()) {
                    (Some(integer), _) => arl_proto::control::ConfigValue::Integer(integer),
                    (None, Some(real)) => arl_proto::control::ConfigValue::Number(real),
                    _ => return Err(failure(ErrorCode::Invalid, "value no representable")),
                },
                _ => {
                    return Err(failure(
                        ErrorCode::Invalid,
                        "value debe ser bool, número o texto",
                    ))
                }
            };
            Ok(ControlMessage::ConfigSet(arl_proto::wire::ConfigSetMsg {
                request_id: request_id.to_owned(),
                expected_revision,
                key,
                value,
            }))
        }
        "capabilities" => Ok(ControlMessage::Hello(arl_proto::wire::HelloMsg {
            request_id: request_id.to_owned(),
            role: PeerRole::LinuxCli,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: arl_proto::wire::SCHEMA_VERSION.to_owned(),
        })),
        _ => Err(failure(ErrorCode::Unsupported, "op desconocida")),
    }
}

fn message_json(message: &ControlMessage) -> Result<Value, String> {
    let frame =
        encode_message(message).map_err(|_| "mensaje de control no codificable".to_owned())?;
    let payload = decode_frame(&frame).map_err(|_| "frame de control inválido".to_owned())?;
    serde_json::from_slice(payload).map_err(|_| "JSON de control inválido".to_owned())
}

fn failure(code: ErrorCode, message: &str) -> Value {
    json!({"code": code.as_str(), "message": message, "retryable": code.retryable()})
}

fn error_reply(id: Value, code: ErrorCode, message: &str) -> Value {
    json!({
        "request_id": id,
        "ok": false,
        "error": {"code": code.as_str(), "message": message},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_provision_quic_stays_off_and_set_is_refused_without_applicator() {
        let (mut runtime, ready) = start(&RuntimeConfig::default()).expect("arranque local");
        assert!(!runtime.quic_enabled());
        assert!(ready.quic_gap.is_some());
        assert!(ready.server_pin.is_none());
        // Sin aplicador de hardware no hay ACK: el set se rechaza y la
        // revisión no se mueve.
        let set = runtime.local_line(
            r#"{"request_id":"t-1","op":"config.set","args":{"key":"audio.mute","value":true,"expected_revision":0}}"#,
        );
        assert_eq!(set["ok"], false, "{set}");
        assert_eq!(set["error"]["code"], "unsupported", "{set}");
        assert_eq!(runtime.revision(), 0);
    }
}
