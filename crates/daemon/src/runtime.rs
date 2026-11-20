//! Proceso de producto: un `SessionServer` y un `SharedControl` en el mismo
//! proceso que el socket Unix del CLI.
//!
//! El admin local entra por `local_line`. El remoto entra por el UDP de la
//! sesión ya autorizada. Los dos ven la misma revisión porque comparten el
//! mismo `SharedControl`. No hay un segundo protocolo ni un puerto en claro.
//!
//! La identidad, la CA local y las identidades aprobadas viven en el
//! directorio de estado (`openrelay::provision`). En el primer arranque se
//! generan ahí; `pair.approve` firma con la clave de la CA la hoja de sesión
//! que el teléfono recibe por el stream de enrolamiento. Sin provisión el
//! proceso arranca sólo con el admin local y deja el endpoint QUIC apagado.

use std::collections::HashMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "return-audio")]
use openrelay_audio_linux::monitor::MonitorSource;
use openrelay_audio_linux::{init as audio_init, PipeWireSource};
#[cfg(feature = "return-audio")]
use openrelay_proto::control::Feature;
use openrelay_proto::control::{ErrorCode, PeerRole, RemoteAction};
use openrelay_proto::wire::{decode_frame, encode_message, ControlMessage};
use openrelay_transport::enrollment::{EnrollOutcome, EnrollReject, EnrollServer, EnrollmentToken};
use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{
    Acl, Capabilities, ServerEvent, SessionId, SessionLimits, SessionServer, TransportError,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use serde_json::{json, Value};

use crate::control::{ControlError, ControlRegistry, IdentityGrant, SharedControl};
use crate::pairing::{self, Pairing};
#[cfg(feature = "return-audio")]
use crate::return_audio::{effective_local_features, should_send, ReturnEmitter, RETURN_EPOCH};
use crate::session::{SessionAudio, SessionAudioStats};

/// Tope de una línea de control que llega por el socket Unix.
const MAX_LINE: usize = 64 * 1024;
/// Cuánto espera el hilo de QUIC un datagrama antes de volver a mirar el paro.
/// También es el período con el que se itera PipeWire.
const READ_TICK: Duration = Duration::from_millis(5);
/// Muestras del anillo de la fuente PipeWire: 200 ms a 48 kHz.
const RING_SAMPLES: usize = 240 * 200;
/// Puerto UDP del enrolamiento. Sólo existe mientras hay una ventana abierta.
pub const ENROLL_PORT: u16 = 47001;

/// Archivos de estado del endpoint de sesión.
///
/// `identity_der` es el certificado del servidor, `identity_key` su PKCS#8,
/// `client_ca_der` la CA que firma a los clientes y `client_ca_key` su clave
/// (para emitir hojas en la aprobación). `identities` es la ACL persistida.
#[derive(Debug, Clone)]
pub struct ProvisionFiles {
    /// Certificado DER del servidor de sesión.
    pub identity_der: PathBuf,
    /// Clave PKCS#8 DER que corresponde a ese certificado.
    pub identity_key: PathBuf,
    /// CA DER que firma los certificados de cliente.
    pub client_ca_der: PathBuf,
    /// Clave PKCS#8 DER de esa CA. El daemon la usa para firmar la hoja de
    /// sesión que devuelve el enrolamiento.
    pub client_ca_key: PathBuf,
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
            local_device_id: "openrelay-cli-local".to_owned(),
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
    /// La ventana de enrolamiento falló al bombear.
    Enroll(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provision(detail) => write!(f, "provision: {detail}"),
            Self::Control(error) => write!(f, "control: {error}"),
            Self::Socket(error) => write!(f, "UDP socket: {error}"),
            Self::Transport(error) => write!(f, "transport: {error}"),
            Self::Enroll(detail) => write!(f, "enrollment: {detail}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

struct QuicEndpoint {
    socket: UdpSocket,
    server: SessionServer,
    /// Identidad TLS del servidor; el enrolamiento reutiliza el mismo
    /// certificado (mismo pin en el QR) y la misma CA local.
    identity: Arc<CertifiedKey>,
    pin: SpkiPin,
    /// Ruta de `identities.json` que escribe la aprobación local.
    identities: PathBuf,
    /// CA y clave con las que se firma la hoja del teléfono.
    client_ca_der: PathBuf,
    client_ca_key: PathBuf,
    /// Fuente `openrelay-mic`. `None` si PipeWire no está disponible: el
    /// daemon sigue sirviendo sesiones y el motivo queda en `pipewire_error`.
    pipewire: Option<PipeWireSource>,
    pipewire_error: Option<String>,
    /// Estado de decodificación por sesión (PCM16 u Opus negociado).
    audio: HashMap<SessionId, SessionAudio>,
    /// Sesiones que este proceso vio autorizadas y aún no cerró.
    live: Vec<SessionId>,
    /// Emisores de retorno Linux→Android: uno por sesión que cumple
    /// `should_send`, con su propia secuencia. Fuera de la feature no existe.
    #[cfg(feature = "return-audio")]
    return_audio: HashMap<SessionId, ReturnEmitter<MonitorSource>>,
    /// Último fallo del retorno (sin sesión PipeWire o sin capacidad). Sólo
    /// informativo: un fallo del retorno no tumba el bombeo del daemon.
    #[cfg(feature = "return-audio")]
    return_gap: Option<String>,
}

/// Ventana de emparejamiento viva: socket UDP efímero + servidor sans-IO.
struct EnrollWindow {
    socket: UdpSocket,
    server: EnrollServer,
    expires: Instant,
    expires_at: u64,
    /// Proceso del CLI que abrió la ventana (`pair.open` con `owner_pid`).
    /// Si desaparece, la ventana se cierra aunque el CLI muriera por SIGINT.
    owner: Option<u32>,
}

/// Proceso dueño del estado. El socket Unix y el UDP de QUIC los atiende el
/// mismo hilo: el estado de control no es `Send` y no se parte en dos.
pub struct ProductRuntime {
    control: SharedControl,
    quic: Option<QuicEndpoint>,
    quic_addr: Option<SocketAddr>,
    /// Ventana de enrolamiento abierta por `pair.open`, si hay una.
    enroll: Option<EnrollWindow>,
    pairing: Pairing,
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
                "provision is missing (identity, CA, identities and grants)".into(),
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
                quic_gap: Some("QUIC endpoint is off: no identity, CA or pin provisioned".into()),
            },
        ),
    };

    Ok((
        ProductRuntime {
            control,
            quic_addr: match quic.as_ref() {
                Some(endpoint) => {
                    Some(endpoint.socket.local_addr().map_err(|error| {
                        RuntimeError::Provision(format!("local address: {error}"))
                    })?)
                }
                None => None,
            },
            quic,
            enroll: None,
            pairing: Pairing::new(),
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
        .map_err(|error| RuntimeError::Provision(format!("server pin: {error}")))?;
    let identity = Arc::new(load_identity(cert, &files.identity_key)?);
    let roots = identity::root_store([read_cert(&files.client_ca_der)?])
        .map_err(|error| RuntimeError::Provision(format!("client CA: {error}")))?;
    let acl = pairing::load(&files.identities).map_err(|error| {
        RuntimeError::Provision(format!(
            "identities ({}): {error}",
            files.identities.display()
        ))
    })?;
    register_grants(registry, &acl, &files.control_grants)?;
    let server = SessionServer::new(Arc::clone(&identity), roots, acl, SessionLimits::default())
        .map_err(RuntimeError::Transport)?;
    let socket = UdpSocket::bind(addr).map_err(RuntimeError::Socket)?;
    let bound = socket.local_addr().map_err(RuntimeError::Socket)?;
    let (pipewire, pipewire_error) = open_pipewire();
    Ok(LoadedQuic {
        endpoint: QuicEndpoint {
            socket,
            server,
            identity,
            pin,
            identities: files.identities.clone(),
            client_ca_der: files.client_ca_der.clone(),
            client_ca_key: files.client_ca_key.clone(),
            pipewire,
            pipewire_error,
            audio: HashMap::new(),
            live: Vec::new(),
            #[cfg(feature = "return-audio")]
            return_audio: HashMap::new(),
            #[cfg(feature = "return-audio")]
            return_gap: None,
        },
        pin: pin.to_hex(),
        addr: bound,
    })
}

/// Abre `openrelay-mic` una vez por proceso. Un fallo no apaga el daemon: el
/// motivo queda en `status.pipewire` y las sesiones siguen aceptándose.
fn open_pipewire() -> (Option<PipeWireSource>, Option<String>) {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(audio_init);
    match PipeWireSource::open(RING_SAMPLES) {
        Ok(mut source) => match source.activate() {
            Ok(()) => (Some(source), None),
            Err(error) => (None, Some(format!("activate source: {error}"))),
        },
        Err(error) => (None, Some(format!("open source: {error}"))),
    }
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
                    "grants line {}: expected pin, device_id and read|write",
                    index + 1
                )))
            }
        };
        let pin = SpkiPin::from_hex(pin_hex).ok_or_else(|| {
            RuntimeError::Provision(format!("grants line {}: invalid pin", index + 1))
        })?;
        let caps = acl.capabilities(&pin).ok_or_else(|| {
            RuntimeError::Provision(format!(
                "grants line {}: pin is not in the loaded ACL",
                index + 1
            ))
        })?;
        let write = match mode {
            "read" => false,
            "write" => true,
            _ => {
                return Err(RuntimeError::Provision(format!(
                    "grants line {}: mode {mode} is neither read nor write",
                    index + 1
                )))
            }
        };
        if write && !caps.control_write() {
            return Err(RuntimeError::Provision(format!(
                "grants line {}: ACL does not grant control_write",
                index + 1
            )));
        }
        if !caps.control_read() {
            return Err(RuntimeError::Provision(format!(
                "grants line {}: ACL does not grant control_read",
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
        RuntimeError::Provision(format!("key ({}): {error}", key_path.display()))
    })?;
    let key_der = PrivatePkcs8KeyDer::from(key_bytes);
    let signing = any_ecdsa_type(&key_der.into())
        .map_err(|_| RuntimeError::Provision("the PKCS#8 key is not usable ECDSA".into()))?;
    // `certified_key` rechaza una clave que no corresponde al certificado.
    identity::certified_key(vec![cert], signing)
        .map_err(|error| RuntimeError::Provision(format!("identity: {error}")))
}

fn read_cert(path: &Path) -> Result<CertificateDer<'static>, RuntimeError> {
    let bytes = fs::read(path).map_err(|error| {
        RuntimeError::Provision(format!("certificate ({}): {error}", path.display()))
    })?;
    Ok(CertificateDer::from(bytes))
}

impl ProductRuntime {
    /// Segundos desde el arranque y sesiones QUIC vivas en este momento.
    pub fn status(&self) -> (u64, u64) {
        let live = self.quic.as_ref().map_or(0, |quic| quic.live.len() as u64);
        (self.started.elapsed().as_secs(), live)
    }

    /// Revisión que ven el admin local y el remoto.
    pub fn revision(&self) -> u64 {
        self.control.revision()
    }

    /// ¿Hay endpoint QUIC en este proceso?
    pub fn quic_enabled(&self) -> bool {
        self.quic.is_some()
    }

    /// Aplica una línea del socket Unix. El JSON es el que ya habla `openrelay`.
    pub fn local_line(&mut self, line: &str) -> Value {
        if line.len() > MAX_LINE {
            return error_reply(Value::Null, ErrorCode::TooLarge, "request too large");
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return error_reply(Value::Null, ErrorCode::Invalid, "invalid JSON"),
        };
        let id = match req.get("request_id") {
            Some(value) if value.is_string() => value.clone(),
            _ => return error_reply(Value::Null, ErrorCode::Invalid, "missing request_id"),
        };
        let op = match req.get("op").and_then(Value::as_str) {
            Some(op) => op,
            None => return error_reply(id, ErrorCode::Invalid, "missing op"),
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
                let pipewire = match self.quic.as_ref() {
                    Some(quic) => match &quic.pipewire_error {
                        Some(error) => json!({"ok": false, "error": error}),
                        None => json!({"ok": true}),
                    },
                    None => json!({"ok": false, "error": "QUIC is off"}),
                };
                Ok(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "uptime_s": uptime_s,
                    "sessions": sessions,
                    "quic": self.quic_addr.map(|addr| addr.to_string()),
                    "pipewire": pipewire,
                    "pair": {
                        "open": self.enroll.is_some(),
                        "pending": self.pairing.pending().len(),
                        "expires_at": self.enroll.as_ref().map(|window| window.expires_at),
                    },
                }))
            }
            "pair.open" => self.pair_open(args),
            "pair.pending" => self.pair_pending(),
            "pair.approve" => self.pair_approve(args),
            "pair.reject" => self.pair_reject(args),
            "pair.close" => self.pair_close(),
            "devices.remove" => self.devices_remove(args),
            "audio.stats" => Ok(self.audio_stats_json()),
            "config.get" | "config.set" | "capabilities" => self.control_op(op, request_id, args),
            other => Err(failure(
                ErrorCode::Unsupported,
                &format!("unknown op: {other}"),
            )),
        }
    }

    fn control_op(&mut self, op: &str, request_id: &str, args: &Value) -> Result<Value, Value> {
        let message = local_message(op, request_id, args)?;
        if encode_message(&message).is_err() {
            return Err(failure(ErrorCode::Invalid, "invalid control request"));
        }
        let outcome = self
            .control
            .local_call(&message)
            .map_err(|_| failure(ErrorCode::Internal, "local admin unavailable"))?;
        let response = outcome
            .response
            .ok_or_else(|| failure(ErrorCode::Internal, "no control response"))?;
        let envelope =
            message_json(&response).map_err(|text| failure(ErrorCode::Internal, &text))?;
        match response {
            ControlMessage::Error(_) => Err(envelope["payload"].clone()),
            _ => Ok(envelope),
        }
    }

    /// Cierra una ventana cuyo CLI dueño ya no existe.
    ///
    /// SIGINT y SIGKILL no ejecutan `Drop`, así que el daemon no puede esperar
    /// un `pair.close`. Se comprueba tanto al abrir como al bombear para que un
    /// `openrelay pair` inmediatamente posterior no vea `busy`.
    fn reap_dead_window(&mut self) -> bool {
        let dead = self
            .enroll
            .as_ref()
            .is_some_and(|window| window.owner.is_some_and(|pid| !pid_alive(pid)));
        if dead {
            self.enroll = None;
            self.pairing.clear_pending();
        }
        dead
    }

    /// Abre una ventana de emparejamiento y devuelve el URI del QR.
    fn pair_open(&mut self, args: &Value) -> Result<Value, Value> {
        self.reap_dead_window();
        if self.enroll.is_some() {
            return Err(failure(ErrorCode::Busy, "a pairing window is already open"));
        }
        let Some(quic) = self.quic.as_ref() else {
            return Err(failure(
                ErrorCode::Unsupported,
                "QUIC is off: no session identity to pair with",
            ));
        };
        let ttl = match args.get("ttl_s") {
            None | Some(Value::Null) => 120,
            Some(value) => match value.as_u64() {
                Some(ttl)
                    if (1..=openrelay_transport::enrollment::MAX_ENROLL_TTL.as_secs())
                        .contains(&ttl) =>
                {
                    ttl
                }
                _ => return Err(failure(ErrorCode::Invalid, "ttl_s out of 1..=300")),
            },
        };
        let host = match args.get("advertise") {
            None | Some(Value::Null) => primary_lan_ipv4()
                .map(IpAddr::V4)
                .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            Some(Value::String(text)) => match text.parse::<IpAddr>() {
                Ok(ip) => ip,
                Err(_) => return Err(failure(ErrorCode::Invalid, "advertise must be an IP")),
            },
            Some(_) => return Err(failure(ErrorCode::Invalid, "advertise must be text")),
        };
        let addr = SocketAddr::new(host, ENROLL_PORT);
        let bind = SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), ENROLL_PORT);
        let socket = UdpSocket::bind(bind)
            .map_err(|error| failure(ErrorCode::Busy, &format!("port {ENROLL_PORT}: {error}")))?;
        let owner = match args.get("owner_pid") {
            None | Some(Value::Null) => None,
            Some(value) => match value.as_u64().and_then(|pid| u32::try_from(pid).ok()) {
                Some(pid) if pid > 0 => Some(pid),
                _ => return Err(failure(ErrorCode::Invalid, "owner_pid must be a PID")),
            },
        };
        let _ = socket.set_nonblocking(true);
        let mut server = EnrollServer::new(Arc::clone(&quic.identity))
            .map_err(|error| failure(ErrorCode::Internal, &format!("enrollment: {error}")))?;
        let token = EnrollmentToken::generate()
            .map_err(|error| failure(ErrorCode::Internal, &format!("token: {error}")))?;
        let now = Instant::now();
        server
            .open_window(token.clone(), Duration::from_secs(ttl), now)
            .map_err(|error| failure(ErrorCode::Internal, &format!("window: {error}")))?;
        let uri = Pairing::qr_payload(quic.pin, addr, &token);
        let expires_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0)
            .saturating_add(ttl);
        self.enroll = Some(EnrollWindow {
            socket,
            server,
            expires: now + Duration::from_secs(ttl),
            expires_at,
            owner,
        });
        Ok(json!({ "uri": uri, "expires_at": expires_at }))
    }

    /// Cierra la ventana sin esperar al TTL y descarta los pendientes.
    ///
    /// Es la salida ordenada de `openrelay pair` (Ctrl-C, EOF o rechazo): la
    /// siguiente ejecución no debe encontrarse `busy`. Si había un teléfono
    /// esperando, se le responde `denied` antes de soltar el socket.
    fn pair_close(&mut self) -> Result<Value, Value> {
        let was_open = self.enroll.is_some();
        if let Some(window) = self.enroll.as_mut() {
            if window.server.pending().is_some() {
                let _ = window
                    .server
                    .reject_pending(Instant::now(), EnrollReject::Denied);
                flush_enroll(window);
            }
        }
        self.enroll = None;
        self.pairing.clear_pending();
        Ok(json!({ "closed": was_open }))
    }

    /// `devices.remove`: borra una identidad aprobada de `identities.json`, la
    /// saca de la ACL viva y cierra las sesiones que tuviera abiertas.
    ///
    /// Es la contraparte de `pair.approve`: sin esto, revocar un teléfono
    /// exigía editar el archivo a mano y reiniciar el daemon.
    fn devices_remove(&mut self, args: &Value) -> Result<Value, Value> {
        let pin = parse_pin(args)?;
        let Some(quic) = self.quic.as_mut() else {
            return Err(failure(
                ErrorCode::Unsupported,
                "QUIC is off: no devices to revoke",
            ));
        };
        let persisted = pairing::load(&quic.identities)
            .map_err(|error| failure(ErrorCode::Internal, &format!("identities: {error}")))?;
        if persisted.capabilities(&pin).is_none() {
            return Err(failure(
                ErrorCode::NotFound,
                "no approved device with that fingerprint",
            ));
        }
        pairing::remove_identity(&quic.identities, &pin)
            .map_err(|error| failure(ErrorCode::Internal, &format!("identities: {error}")))?;
        let closed = quic
            .live
            .iter()
            .filter(|session| quic.server.identity(**session) == Some(pin))
            .count();
        quic.server.revoke(&pin, Instant::now());
        // Los grants de control viven en otro registro: sin esto el pin seguiría
        // con permisos de lectura/escritura aunque la ACL de sesión ya lo negara.
        self.control.registry_mut().revoke(&pin);
        Ok(json!({
            "fingerprint_hex": pin.to_hex(),
            "removed": true,
            "closed_sessions": closed,
        }))
    }

    fn pair_pending(&self) -> Result<Value, Value> {
        let pending: Vec<Value> = self
            .pairing
            .pending()
            .iter()
            .map(|pin| json!({"fingerprint_hex": pin.to_hex()}))
            .collect();
        Ok(Value::Array(pending))
    }

    /// Aprueba el dispositivo pendiente, firma su hoja y la entrega por el
    /// stream de enrolamiento. La sesión siguiente ya entra sin reiniciar.
    fn pair_approve(&mut self, args: &Value) -> Result<Value, Value> {
        let pin = parse_pin(args)?;
        if !self.pairing.pending().contains(&pin) {
            return Err(failure(ErrorCode::NotFound, "identity is not pending"));
        }
        let Some(enroll) = self.enroll.as_mut() else {
            return Err(failure(ErrorCode::NotFound, "no pairing window"));
        };
        let spki = enroll
            .server
            .pending_spki()
            .ok_or_else(|| failure(ErrorCode::NotFound, "the device did not send its SPKI"))?
            .to_vec();
        let quic = self
            .quic
            .as_mut()
            .ok_or_else(|| failure(ErrorCode::Unsupported, "QUIC is off"))?;
        let ca = crate::issue::load(&quic.client_ca_der, &quic.client_ca_key)
            .map_err(|error| failure(ErrorCode::Internal, &error))?;
        let client_leaf_der = crate::issue::issue_client_leaf(&ca, &spki)
            .map_err(|error| failure(ErrorCode::Invalid, &error))?;
        let session_ca_der = fs::read(&quic.client_ca_der)
            .map_err(|error| failure(ErrorCode::Internal, &format!("CA: {error}")))?;
        let session_port = quic
            .socket
            .local_addr()
            .map_err(|error| failure(ErrorCode::Internal, &format!("QUIC socket: {error}")))?
            .port();
        let caps = Capabilities::none()
            .with_mic()
            .with_control_read()
            .with_control_write();
        self.pairing
            .approve(pin, caps, &quic.identities)
            .map_err(|error| failure(ErrorCode::Internal, &format!("identities: {error}")))?;
        quic.server.allow(pin, caps);
        let outcome = EnrollOutcome {
            client_leaf_der,
            session_ca_der,
            session_port,
        };
        enroll
            .server
            .approve(Instant::now(), &outcome)
            .map_err(|error| failure(ErrorCode::Internal, &format!("response: {error}")))?;
        flush_enroll(enroll);
        // La hoja va por QUIC: el cliente necesita ACKs y retransmisiones para
        // recibirla. Cerrar el socket aquí perdería la respuesta; la ventana se
        // queda unos segundos y `pump_enroll` la retira al vencer. El dueño deja
        // de vigilarse: el CLI ya termina y la ventana debe sobrevivirle.
        enroll.expires = Instant::now() + Duration::from_secs(5);
        enroll.owner = None;
        Ok(json!({
            "fingerprint_hex": pin.to_hex(),
            "approved": true,
            "session_port": session_port,
        }))
    }

    /// Rechaza el pendiente (o todos si no se nombra uno) y cierra la ventana.
    fn pair_reject(&mut self, args: &Value) -> Result<Value, Value> {
        let pin = match args.get("fingerprint_hex") {
            Some(Value::String(text)) => {
                SpkiPin::from_hex(text).ok_or_else(|| failure(ErrorCode::Invalid, "invalid pin"))?
            }
            _ => match self.pairing.pending().first() {
                Some(pin) => *pin,
                None => return Err(failure(ErrorCode::NotFound, "no pending identity")),
            },
        };
        let Some(enroll) = self.enroll.as_mut() else {
            return Err(failure(ErrorCode::NotFound, "no pairing window"));
        };
        enroll
            .server
            .reject_pending(
                Instant::now(),
                openrelay_transport::enrollment::EnrollReject::Denied,
            )
            .map_err(|error| failure(ErrorCode::Internal, &format!("response: {error}")))?;
        flush_enroll(enroll);
        self.pairing.reject(pin);
        enroll.expires = Instant::now() + Duration::from_secs(5);
        Ok(json!({"fingerprint_hex": pin.to_hex(), "rejected": true}))
    }

    /// Una vuelta al socket de enrolamiento: sólo existe mientras la ventana
    /// está abierta. Los eventos alimentan `pair.pending`.
    fn pump_enroll(&mut self) -> Result<(), RuntimeError> {
        if self.reap_dead_window() {
            // El CLI que abrió la ventana murió sin llamar a `pair.close`
            // (SIGINT, SIGKILL o cierre de la terminal): la ventana no debe
            // sobrevivirle ni bloquear el siguiente `openrelay pair`.
            return Ok(());
        }
        let Some(window) = self.enroll.as_mut() else {
            return Ok(());
        };
        let now = Instant::now();
        let mut buf = [0u8; 2048];
        loop {
            match window.socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let _ = window.server.handle_datagram(now, from, &buf[..len]);
                }
                Err(error) if is_timeout(&error) => break,
                Err(error) => return Err(RuntimeError::Socket(error)),
            }
        }
        while let Some(event) = window.server.next_event() {
            self.pairing.observe(&event);
        }
        window
            .server
            .handle_timeout(now)
            .map_err(|error| RuntimeError::Enroll(error.to_string()))?;
        flush_enroll(window);
        let expired = window.expires <= now && window.server.pending().is_none();
        if expired {
            self.enroll = None;
            self.pairing.clear_pending();
        }
        Ok(())
    }

    fn audio_stats_json(&self) -> Value {
        let Some(quic) = self.quic.as_ref() else {
            return json!({"sessions": []});
        };
        let sessions: Vec<Value> = quic
            .live
            .iter()
            .filter_map(|session| {
                let stats = quic.audio.get(session)?.stats();
                Some(json!({
                    "session": session.get(),
                    "pin": quic.server.identity(*session).map(|pin| pin.to_hex()),
                    "pcm16_frames": stats.pcm16_frames,
                    "opus_frames": stats.opus_frames,
                    "plc_frames": stats.plc_frames,
                    "dropped_unnegotiated_opus": stats.dropped_unnegotiated_opus,
                    "dropped_duplicates": stats.dropped_duplicates,
                    "dropped_reordered": stats.dropped_reordered,
                    "dropped_cadence": stats.dropped_cadence,
                    "discontinuities": stats.discontinuities,
                    "epoch_resets": stats.epoch_resets,
                    "decode_errors": stats.decode_errors,
                    "ns_stages": stats.ns_stages,
                    "ns_frames_processed": stats.ns_frames_processed,
                    "ns_frames_bypassed": stats.ns_frames_bypassed,
                    "ns_added_latency_samples": stats.ns_added_latency_samples,
                }))
            })
            .collect();
        json!({"sessions": sessions})
    }

    /// Una vuelta del endpoint QUIC: lee, autoriza, responde control y drena audio.
    ///
    /// El audio se decodifica en este hilo, el worker del daemon, y desde aquí
    /// se escribe el anillo que consume PipeWire. La reducción de ruido
    /// opcional, si la sesión se construyó con `SessionAudio::with_ns`, corre en
    /// esta misma vuelta antes del anillo: el callback de tiempo real nunca
    /// ejecuta DSP.
    pub fn pump_quic(&mut self) -> Result<(), RuntimeError> {
        self.pump_enroll()?;
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
        let now_us = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        quic.server
            .handle_timeout(now)
            .map_err(RuntimeError::Transport)?;
        flush_transmit(&quic.socket, &mut quic.server)?;

        while let Some(event) = quic.server.next_event() {
            match event {
                ServerEvent::Authorized { session, .. } => {
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
                pipewire,
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
                if let Ok(frames) = state.handle(payload, now_us) {
                    if let Some(source) = pipewire.as_mut() {
                        for frame in frames {
                            let _ = source.producer().push_slice(&frame.samples);
                        }
                    }
                }
            }
        }
        // El bucle principal de PipeWire se atiende en este hilo; el callback
        // RT sólo consume el anillo.
        if let Some(source) = quic.pipewire.as_ref() {
            let _ = source.iterate(Duration::ZERO);
        }
        #[cfg(feature = "return-audio")]
        pump_return_audio(&self.control, quic);
        Ok(())
    }

    /// Último fallo del retorno Linux→Android (`None` si nunca falló o si el
    /// daemon se compiló sin la feature).
    #[cfg(feature = "return-audio")]
    pub fn return_gap(&self) -> Option<&str> {
        self.quic.as_ref()?.return_gap.as_deref()
    }
    pub fn audio_stats(&self, session: SessionId) -> Option<SessionAudioStats> {
        self.quic
            .as_ref()?
            .audio
            .get(&session)
            .map(SessionAudio::stats)
    }
}

/// Sesiones elegibles para el retorno en esta vuelta.
///
/// Es la puerta que usa el runtime, no `should_send` a secas: las features
/// locales efectivas salen de la config (`audio.return_enabled`), así que el
/// daemon no se atribuye una capacidad que tiene apagada. Sin mensaje
/// `capabilities` del peer tampoco hay retorno.
#[cfg(feature = "return-audio")]
fn return_targets<S, F>(live: &[S], enabled: bool, mut peer_features: F) -> Vec<S>
where
    S: Copy + Eq,
    F: FnMut(S) -> Option<Vec<Feature>>,
{
    let local = effective_local_features(enabled);
    let mut targets = Vec::new();
    for session in live.iter().copied() {
        if peer_features(session).is_some_and(|peer| should_send(&peer, &local, enabled)) {
            targets.push(session);
        }
    }
    targets
}

/// Una vuelta del retorno Linux→Android.
///
/// Sólo actúa en sesiones que cumplen la puerta [`return_targets`]: la feature
/// anunciada por el peer, la local efectiva (derivada de
/// `audio.return_enabled`) y la config encendida. El daemon mantiene **un solo
/// `MonitorSource`** y sirve el retorno a **una sola sesión a la vez** (ver
/// `docs/return-audio.md`): repartir el mismo anillo entre varios peers dejaría
/// a todos con huecos, así que las demás esperan a que la primera cierre. La
/// secuencia de cada emisor no se mezcla con la de otro ni con el micrófono.
/// Sin sesión de PipeWire el emisor no se crea y el hueco queda en
/// `return_gap`; el bombeo del daemon sigue.
#[cfg(feature = "return-audio")]
fn pump_return_audio(control: &SharedControl, quic: &mut QuicEndpoint) {
    let enabled = control.return_enabled();
    let targets = return_targets(&quic.live, enabled, |session| {
        quic.server
            .identity(session)
            .and_then(|pin| control.peer_features(&pin).map(<[Feature]>::to_vec))
    });
    let target = targets.first().copied();
    quic.return_audio
        .retain(|session, _| Some(*session) == target);
    if let Some(session) = target {
        // Toma el emisor de la sesión o abre el único `MonitorSource` del
        // daemon; `retain` de arriba garantiza que no quede otro.
        let mut emitter = match quic.return_audio.remove(&session) {
            Some(emitter) => emitter,
            None => {
                let mut source = match MonitorSource::open(RING_SAMPLES) {
                    Ok(source) => source,
                    Err(error) => {
                        quic.return_gap = Some(error.to_string());
                        return;
                    }
                };
                if let Err(error) = source.activate() {
                    quic.return_gap = Some(error.to_string());
                    return;
                }
                match ReturnEmitter::new(source, RETURN_EPOCH, true) {
                    Ok(emitter) => {
                        quic.return_gap = None;
                        emitter
                    }
                    Err(error) => {
                        quic.return_gap = Some(error.to_string());
                        return;
                    }
                }
            }
        };
        if let Some(frame) = emitter.peek() {
            // Un fallo de envío no tumba el bombeo ni consume la secuencia:
            // el frame se descarta y el próximo reusa el número, sin hueco
            // falso.
            match quic.server.send_audio(session, &frame) {
                Ok(()) => emitter.commit(),
                Err(error) => quic.return_gap = Some(error.to_string()),
            }
        }
        quic.return_audio.insert(session, emitter);
    }
}

fn flush_transmit(socket: &UdpSocket, server: &mut SessionServer) -> Result<(), RuntimeError> {
    let now = Instant::now();
    while let Some((to, bytes)) = server.poll_transmit(now) {
        socket.send_to(&bytes, to).map_err(RuntimeError::Socket)?;
    }
    Ok(())
}

/// Envía lo que el servidor de enrolamiento tenga listo. El socket es no
/// bloqueante: un búfer lleno no debe tumbar el daemon.
fn flush_enroll(window: &mut EnrollWindow) {
    let now = Instant::now();
    while let Some((to, bytes)) = window.server.poll_transmit(now) {
        match window.socket.send_to(&bytes, to) {
            Ok(_) => {}
            Err(error) if is_timeout(&error) => break,
            Err(_) => break,
        }
    }
}

fn parse_pin(args: &Value) -> Result<SpkiPin, Value> {
    let text = args
        .get("fingerprint_hex")
        .and_then(Value::as_str)
        .ok_or_else(|| failure(ErrorCode::Invalid, "missing fingerprint_hex"))?;
    SpkiPin::from_hex(text).ok_or_else(|| failure(ErrorCode::Invalid, "invalid pin"))
}

/// IPv4 no loopback de la interfaz que usaría el kernel para salir a la LAN.
/// No envía tráfico: conecta el socket UDP y lee la dirección local elegida.
fn primary_lan_ipv4() -> Option<std::net::Ipv4Addr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// ¿Sigue vivo el proceso `pid`? Linux expone cada proceso como `/proc/<pid>`;
/// un PID reciclado sólo podría alargar una ventana hasta su TTL, nunca abrirla.
fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn local_message(op: &str, request_id: &str, args: &Value) -> Result<ControlMessage, Value> {
    match op {
        "config.get" => {
            let key = match args.get("key") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) => Some(
                    openrelay_proto::control::ConfigKey::new(name)
                        .map_err(|code| failure(code, "invalid config key"))?,
                ),
                Some(_) => return Err(failure(ErrorCode::Invalid, "key must be text")),
            };
            Ok(ControlMessage::ConfigGet(
                openrelay_proto::wire::ConfigGetMsg {
                    request_id: request_id.to_owned(),
                    key,
                },
            ))
        }
        "config.set" => {
            let key = match args.get("key").and_then(Value::as_str) {
                Some(name) => openrelay_proto::control::ConfigKey::new(name)
                    .map_err(|code| failure(code, "invalid config key"))?,
                None => return Err(failure(ErrorCode::Invalid, "missing key")),
            };
            let expected_revision = args
                .get("expected_revision")
                .and_then(Value::as_u64)
                .ok_or_else(|| failure(ErrorCode::Invalid, "missing expected_revision"))?;
            let value = match args.get("value") {
                Some(Value::Bool(flag)) => openrelay_proto::control::ConfigValue::Bool(*flag),
                Some(Value::String(text)) => {
                    openrelay_proto::control::ConfigValue::String(text.clone())
                }
                Some(Value::Number(number)) => match (number.as_i64(), number.as_f64()) {
                    (Some(integer), _) => openrelay_proto::control::ConfigValue::Integer(integer),
                    (None, Some(real)) => openrelay_proto::control::ConfigValue::Number(real),
                    _ => return Err(failure(ErrorCode::Invalid, "value is not representable")),
                },
                _ => {
                    return Err(failure(
                        ErrorCode::Invalid,
                        "value must be a bool, number or text",
                    ))
                }
            };
            Ok(ControlMessage::ConfigSet(
                openrelay_proto::wire::ConfigSetMsg {
                    request_id: request_id.to_owned(),
                    expected_revision,
                    key,
                    value,
                },
            ))
        }
        "capabilities" => Ok(ControlMessage::Hello(openrelay_proto::wire::HelloMsg {
            request_id: request_id.to_owned(),
            role: PeerRole::LinuxCli,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: openrelay_proto::wire::SCHEMA_VERSION.to_owned(),
        })),
        _ => Err(failure(ErrorCode::Unsupported, "unknown op")),
    }
}

fn message_json(message: &ControlMessage) -> Result<Value, String> {
    let frame =
        encode_message(message).map_err(|_| "control message is not encodable".to_owned())?;
    let payload = decode_frame(&frame).map_err(|_| "invalid control frame".to_owned())?;
    serde_json::from_slice(payload).map_err(|_| "invalid control JSON".to_owned())
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

    /// Con `audio.return_enabled = false` el runtime no elige ninguna sesión
    /// aunque el peer anuncie el retorno: no se abre el monitor ni se emite.
    /// La puerta deriva las features locales efectivas de la config.
    #[cfg(feature = "return-audio")]
    #[test]
    fn runtime_no_elige_retorno_si_la_config_lo_apaga() {
        use openrelay_proto::control::Feature;
        let peer = [Feature::AudioPcm16_48kMono, Feature::AudioReturnPcm48kMono];
        assert!(
            return_targets(&[7u64, 8], false, |_| Some(peer.to_vec())).is_empty(),
            "config off: an announcing peer is not enough"
        );
        assert_eq!(
            return_targets(&[7u64, 8], true, |_| Some(peer.to_vec())),
            vec![7, 8],
            "config on and announcing peer: it is included"
        );
        assert!(
            return_targets(&[7u64], true, |_| None).is_empty(),
            "without peer capabilities there is no return audio"
        );
    }

    #[test]
    fn pair_close_without_a_window_is_a_noop() {
        let (mut runtime, _) = start(&RuntimeConfig::default()).expect("local start");
        let closed = runtime.local_line(r#"{"request_id":"t-2","op":"pair.close"}"#);
        assert_eq!(closed["ok"], true, "{closed}");
        assert_eq!(closed["result"]["closed"], false, "{closed}");
    }

    #[test]
    fn pid_alive_sees_this_process_and_not_a_missing_one() {
        assert!(pid_alive(std::process::id()), "this process exists");
        assert!(!pid_alive(u32::MAX), "an impossible PID does not exist");
    }

    #[test]
    fn without_provision_quic_stays_off_and_set_is_refused_without_applicator() {
        let (mut runtime, ready) = start(&RuntimeConfig::default()).expect("local start");
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
