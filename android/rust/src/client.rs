//! Cliente de sesión de producto (F4): UDP real, control `openrelay.control/v1`
//! y cierre local. La clave privada no entra aquí: la firma la pide
//! [`super::KeystoreKey`] al `KeystoreSigner` de Java.
//!
//! No hay servidor embebido, CA de test, TOFU ni `verify=false`. La
//! provisión (endpoint, nombre, ancla DER, pin SPKI y cadena del cliente)
//! la entrega quien llama. El ACK de config no acredita audio físico y
//! este módulo no abre el micrófono.

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use openrelay_audio_core::AudioPayload;
use openrelay_proto::control::{
    Capabilities as PeerCapabilities, ConfigKey, ConfigValue, ErrorMsg, Feature, PeerRole,
    RemoteAction,
};
use openrelay_proto::wire::{
    encode_message, AckEffective, AckMsg, CapabilitiesMsg, ConfigGetMsg, ConfigSetMsg,
    ControlMessage, HelloMsg, SCHEMA_VERSION,
};
use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{ClientEvent, SessionClient, SessionLimits};
use rustls::pki_types::CertificateDer;

use crate::audio::{local_features, selected_format, AudioSender, CodecChoice};
use crate::return_audio::ReturnQueue;

use rustls::sign::SigningKey;

/// Datagrama UDP máximo que se lee del socket (QUIC cabe holgado).
const UDP_BUF: usize = 2048;
/// Tope de la cola de eventos hacia Java. Lo que no cabe se descarta.
const EVENT_CAP: usize = 32;
/// Cuánto espera el hilo de IO un datagrama antes de mirar timeouts.
const RECV_SLICE: Duration = Duration::from_millis(50);
/// Tope de un roundtrip de control. Pasado, la llamada devuelve timeout.
const CONTROL_DEADLINE: Duration = Duration::from_secs(5);
/// Espera del handshake antes de declarar que no hubo autorización.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(8);
/// Versión de la app que viaja en `hello`.
const APP_VERSION: &str = "0.2.0";
/// `device_id` estable que el puente declara en `capabilities`.
const DEVICE_ID: &str = "android-bridge";

/// Evento que Java puede leer. Texto corto, sin secretos ni rutas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BridgeEvent {
    /// Handshake completo: ALPN de sesión y pin del servidor verificados.
    Authorized,
    /// El handshake no superó la frontera (cadena, pin, ALPN o ACL).
    Rejected,
    /// Cierre observado en el transporte, o cierre local ya aplicado.
    Closed,
    /// `ack` de control. `requested` y `effective` son JSON; pueden diferir.
    Ack {
        request_id: String,
        revision: u64,
        applied: bool,
        requested_json: String,
        effective_json: String,
    },
    /// `error` de control. No es un ACK y no cambia el audio.
    /// `current_revision` sólo existe en un conflicto de revisión.
    Error {
        request_id: String,
        code: String,
        current_revision: Option<u64>,
    },
    /// El hilo de IO murió por un fallo de socket, no por un cierre pedido.
    IoFault(String),
}

struct Shared {
    client: SessionClient,
    socket: UdpSocket,
    server: SocketAddr,
    events: Vec<BridgeEvent>,
    /// `request_id` del hello, para no publicar su capabilities como ACK.
    hello_id: String,
    /// Features que el servidor anunció en su `capabilities` (texto del
    /// contrato: p. ej. `audio.opus-48k-mono`).
    server_features: Vec<String>,
    /// Revisión que el servidor reportó en su último `capabilities`.
    server_revision: u64,
    /// `true` una vez que el `hello` salió; antes no se anuncian features.
    hello_sent: bool,
    /// Perfil y anuncio de retorno con los que ya se encolaron las features
    /// al servidor en esta sesión.
    advertised: Option<(CodecChoice, bool)>,
    /// Perfil de envío y codificador: PCM16 por defecto, Opus si se negocia.
    audio: AudioSender,
    /// Cola acotada del retorno Linux→Android; apagada por defecto.
    return_audio: ReturnQueue,
    /// `true` cuando Java pidió el cierre: el hilo sale sin emitir `IoFault`.
    stop: bool,
}

impl Shared {
    /// Encola las `capabilities` propias cuando el perfil activo cambió.
    ///
    /// El daemon sólo acepta frames Opus de un peer que anunció
    /// `audio.opus-48k-mono`; PCM16 también se declara. Sin `hello` previo o
    /// sin autorización no se envía nada y se reintenta en el siguiente bombeo.
    fn advertise(&mut self) -> Result<(), String> {
        if !self.hello_sent || !self.client.is_authorized() {
            return Ok(());
        }
        let choice = self.audio.choice();
        let returned = self.return_audio.enabled();
        if self.advertised == Some((choice, returned)) {
            return Ok(());
        }
        let mut capabilities = PeerCapabilities::from_peer(
            DEVICE_ID,
            PeerRole::Android,
            vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
        );
        let mut features: Vec<Feature> = local_features(choice)
            .into_iter()
            .map(|name| match name {
                openrelay_audio_core::FEATURE_AUDIO_OPUS => Feature::AudioOpus48kMono,
                _ => Feature::AudioPcm16_48kMono,
            })
            .collect();
        // El retorno sólo se declara si Java lo pidió: por defecto no viaja.
        if returned {
            features.push(Feature::AudioReturnPcm48kMono);
        }
        capabilities.features = features;
        let message = ControlMessage::Capabilities(CapabilitiesMsg {
            request_id: request_id(),
            revision: self.server_revision,
            capabilities,
        });
        self.client.send_control(&message).map_err(show)?;
        self.advertised = Some((choice, returned));
        Ok(())
    }
}

/// Handle de producto. El mapa de JNI guarda `Arc<Mutex<…>>`, nunca un puntero.
pub(crate) struct ProductClient {
    shared: Arc<Mutex<Shared>>,
    worker: Option<JoinHandle<()>>,
    closed: AtomicBool,
}

/// Provisión ya validada en tipos de Rust. Java entrega bytes y texto.
pub(crate) struct Provision {
    pub endpoint: SocketAddr,
    pub server_name: String,
    pub ca_der: CertificateDer<'static>,
    pub pin: SpkiPin,
    pub chain: Vec<CertificateDer<'static>>,
    pub key: Arc<dyn SigningKey>,
}

impl ProductClient {
    /// Abre el UDP, arranca el handshake y el hilo de IO. No bloquea en red.
    pub(crate) fn open(provision: Provision) -> Result<Self, String> {
        let roots = identity::root_store([provision.ca_der]).map_err(show)?;
        let identity =
            Arc::new(identity::certified_key(provision.chain, provision.key).map_err(show)?);
        let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).map_err(show)?;
        socket.connect(provision.endpoint).map_err(show)?;
        socket.set_read_timeout(Some(RECV_SLICE)).map_err(show)?;
        let local = socket.local_addr().map_err(show)?;
        let client = SessionClient::connect(
            identity,
            roots,
            provision.pin,
            SessionLimits::default(),
            Instant::now(),
            provision.endpoint,
            &provision.server_name,
        )
        .map_err(show)?;
        let hello_id = request_id();
        let shared = Arc::new(Mutex::new(Shared {
            client,
            socket,
            server: provision.endpoint,
            events: Vec::new(),
            hello_id,
            server_features: Vec::new(),
            server_revision: 0,
            hello_sent: false,
            advertised: None,
            audio: AudioSender::new(),
            return_audio: ReturnQueue::new(),
            stop: false,
        }));
        // El primer vuelo sale ya: si no, el servidor no tiene a quién contestar.
        drive(&shared, local)?;
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("openrelay-session-io".into())
            .spawn(move || io_loop(worker_shared, local))
            .map_err(show)?;
        Ok(Self {
            shared,
            worker: Some(worker),
            closed: AtomicBool::new(false),
        })
    }

    /// `hello` de rol android y, acto seguido, nuestras `capabilities`. La
    /// respuesta del daemon no es un ACK de config.
    pub(crate) fn hello(&self) -> Result<(), String> {
        let message = ControlMessage::Hello(HelloMsg {
            request_id: {
                let guard = self
                    .shared
                    .lock()
                    .map_err(|_| "lock envenenado".to_string())?;
                guard.hello_id.clone()
            },
            role: PeerRole::Android,
            version: APP_VERSION.into(),
            protocol_version: SCHEMA_VERSION.into(),
        });
        self.send_only(&message)?;
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        guard.hello_sent = true;
        guard.advertised = None;
        guard.advertise()
    }

    /// `config.get`. `key = None` pide el snapshot. Devuelve el `request_id`.
    pub(crate) fn config_get(&self, key: Option<&str>) -> Result<String, String> {
        let request_id = request_id();
        let key = match key {
            Some(name) if !name.is_empty() => {
                Some(ConfigKey::new(name).map_err(|e| format!("{e:?}"))?)
            }
            _ => None,
        };
        let message = ControlMessage::ConfigGet(ConfigGetMsg {
            request_id: request_id.clone(),
            key,
        });
        self.send_only(&message)?;
        Ok(request_id)
    }

    /// `config.set` de `audio.gain_db` (número finito). No aplica nada en local.
    pub(crate) fn config_set_gain(
        &self,
        expected_revision: u64,
        gain_db: f64,
    ) -> Result<String, String> {
        if !gain_db.is_finite() {
            return Err("gain_db no es finito".into());
        }
        let request_id = request_id();
        let message = ControlMessage::ConfigSet(ConfigSetMsg {
            request_id: request_id.clone(),
            expected_revision,
            key: ConfigKey::new("audio.gain_db").map_err(|e| format!("{e:?}"))?,
            value: ConfigValue::Number(gain_db),
        });
        self.send_only(&message)?;
        Ok(request_id)
    }

    /// Fija el perfil de envío: `0` PCM16 (defecto), `1` Opus 5 ms, `2`
    /// 2,5 ms, `3` Opus 10 ms. Si cambia, se anuncian las features locales;
    /// sin servidor que también anuncie `audio.opus-48k-mono`, sigue PCM16.
    pub(crate) fn set_codec(&self, profile: i32) -> Result<(), String> {
        let choice = CodecChoice::from_jint(profile)?;
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        let changed = guard.audio.choice() != choice;
        guard.audio.set_choice(choice)?;
        if changed {
            guard.advertise()?;
        }
        Ok(())
    }

    /// Enciende o apaga el retorno y vuelve a anunciar `capabilities`.
    ///
    /// Apagado (defecto) la feature `audio.return-pcm16-48k-mono` no se
    /// declara y no se encola ningún datagrama que llegue. Al encenderlo se
    /// vacía lo que hubiera y se reanuncia: el daemon sólo emite si ambos
    /// extremos lo ven.
    pub(crate) fn set_return_audio(&self, enabled: bool) -> Result<(), String> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        guard.return_audio.set_enabled(enabled);
        guard.advertised = None;
        guard.advertise()
    }

    /// Saca un frame de retorno ya decodificado. `0` si la cola está vacía.
    pub(crate) fn read_return(&self, dst: &mut [i16]) -> Result<usize, String> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        Ok(guard.return_audio.read_into(dst))
    }

    /// Envía un bloque de muestras del perfil activo como un frame de audio.
    ///
    /// PCM16 exige 240 muestras (5 ms); Opus, las de su perfil. El frame sale
    /// de `selected_format`: PCM16 por defecto, y Opus sólo si el servidor lo
    /// anuncia y nuestras `capabilities` ya declararon el perfil.
    pub(crate) fn send_audio(&self, samples: &[i16]) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("sesión cerrada".into());
        }
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        if guard.stop {
            return Err("sesión cerrada".into());
        }
        if !guard.client.is_authorized() {
            return Err("sesión no autorizada".into());
        }
        let choice = guard.audio.choice();
        let format = selected_format(
            choice,
            &guard.server_features,
            guard.advertised == Some((choice, guard.return_audio.enabled())),
        );
        let payload = guard.audio.encode(format, samples)?;
        match payload {
            // PCM16 sale por el mismo `send_audio` de siempre: byte a byte igual.
            AudioPayload::Pcm16(frame) => guard.client.send_audio(&frame).map_err(show),
            payload @ AudioPayload::Opus(_) => {
                guard.client.send_audio_payload(&payload).map_err(show)
            }
        }
    }

    /// Siguiente evento, o `None` si la cola está vacía. No bloquea.
    pub(crate) fn poll_event(&self) -> Result<Option<BridgeEvent>, String> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        if guard.events.is_empty() {
            Ok(None)
        } else {
            Ok(Some(guard.events.remove(0)))
        }
    }

    /// Espera un `ack` o `error` con ese `request_id`, o timeout.
    pub(crate) fn wait_control(&self, request_id: &str) -> Result<BridgeEvent, String> {
        let deadline = Instant::now() + CONTROL_DEADLINE;
        loop {
            if let Some(event) = self.take_matching(request_id)? {
                return Ok(event);
            }
            if Instant::now() >= deadline {
                return Err("timeout de control".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Espera la autorización TLS o un rechazo. No envía audio.
    pub(crate) fn wait_authorized(&self) -> Result<(), String> {
        let deadline = Instant::now() + HANDSHAKE_DEADLINE;
        loop {
            let event = {
                let mut guard = self
                    .shared
                    .lock()
                    .map_err(|_| "lock envenenado".to_string())?;
                pull_terminal(&mut guard.events)
            };
            match event {
                Some(BridgeEvent::Authorized) => return Ok(()),
                Some(BridgeEvent::Rejected) => return Err("handshake rechazado".into()),
                Some(BridgeEvent::Closed) => return Err("sesión cerrada antes de autorizar".into()),
                Some(BridgeEvent::IoFault(detail)) => {
                    return Err(format!("fallo de IO antes de autorizar: {detail}"));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err("timeout de handshake".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Cierre local: para el hilo, suelta el socket y deja el handle inservible.
    /// Un segundo cierre es idempotente. No pide permiso de micrófono.
    pub(crate) fn close(&mut self) -> Result<(), String> {
        if self
            .closed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(());
        }
        {
            let mut guard = self
                .shared
                .lock()
                .map_err(|_| "lock envenenado".to_string())?;
            guard.stop = true;
            guard.events.push(BridgeEvent::Closed);
        }
        if let Some(worker) = self.worker.take() {
            // El hilo sale en el siguiente corte de recepción (≤ RECV_SLICE).
            let _ = worker.join();
        }
        Ok(())
    }

    fn send_only(&self, message: &ControlMessage) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("sesión cerrada".into());
        }
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        if guard.stop {
            return Err("sesión cerrada".into());
        }
        if !guard.client.is_authorized() {
            return Err("sesión no autorizada".into());
        }
        guard.client.send_control(message).map_err(show)?;
        // El hilo de IO drena `poll_transmit`; no se escribe el socket aquí
        // para no competir con él por el mismo descriptor.
        Ok(())
    }

    fn take_matching(&self, request_id: &str) -> Result<Option<BridgeEvent>, String> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| "lock envenenado".to_string())?;
        if let Some(index) = guard.events.iter().position(|event| match event {
            BridgeEvent::Ack { request_id: id, .. } | BridgeEvent::Error { request_id: id, .. } => {
                id == request_id
            }
            _ => false,
        }) {
            return Ok(Some(guard.events.remove(index)));
        }
        if guard.events.iter().any(|event| {
            matches!(
                event,
                BridgeEvent::Closed | BridgeEvent::IoFault(_) | BridgeEvent::Rejected
            )
        }) {
            return Err("sesión cerrada durante el control".into());
        }
        Ok(None)
    }
}

impl Drop for ProductClient {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Hilo de fondo: lee UDP, entrega al sans-IO, drena respuestas y publica eventos.
fn io_loop(shared: Arc<Mutex<Shared>>, local: SocketAddr) {
    let mut buf = [0u8; UDP_BUF];
    // El socket se clona para esperar el datagrama sin el candado: sostenerlo
    // durante el `recv` bloqueado deja a `wait_control`/`wait_authorized` sin
    // turno durante segundos y agota sus plazos. El original se queda en
    // `Shared` para los envíos de `drive`.
    let socket = match shared.lock() {
        Ok(guard) => match guard.socket.try_clone() {
            Ok(socket) => socket,
            Err(_) => return,
        },
        Err(_) => return,
    };
    loop {
        {
            let guard = match shared.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            if guard.stop {
                return;
            }
        }
        let recv = socket.recv(&mut buf);
        match recv {
            Ok(n) => {
                let mut guard = match shared.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if guard.stop {
                    return;
                }
                let payload = buf[..n].to_vec();
                let server = guard.server;
                if guard
                    .client
                    .handle_datagram(Instant::now(), server, &payload)
                    .is_err()
                {
                    push_event(&mut guard, BridgeEvent::IoFault("handle_datagram".into()));
                    guard.stop = true;
                    return;
                }
                publish(&mut guard);
            }
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut => {
            }
            Err(error) => {
                let mut guard = match shared.lock() {
                    Ok(guard) => guard,
                    Err(_) => return,
                };
                if !guard.stop {
                    push_event(&mut guard, BridgeEvent::IoFault(format!("recv {error}")));
                    guard.stop = true;
                }
                return;
            }
        }
        if drive(&shared, local).is_err() {
            return;
        }
    }
}

/// Aplica timeouts, publica eventos y escribe lo que el sans-IO quiera enviar.
fn drive(shared: &Arc<Mutex<Shared>>, local: SocketAddr) -> Result<(), String> {
    let mut guard = shared.lock().map_err(|_| "lock envenenado".to_string())?;
    if guard.stop {
        return Ok(());
    }
    let now = Instant::now();
    if guard.client.handle_timeout(now).is_err() {
        push_event(&mut guard, BridgeEvent::IoFault("handle_timeout".into()));
        guard.stop = true;
        return Err("timeout de transporte".into());
    }
    publish(&mut guard);
    let mut batch = Vec::new();
    while let Some((destination, payload)) = guard.client.poll_transmit(now) {
        // El socket está conectado al servidor de la provisión: no se reenvía
        // a otra dirección aunque el sans-IO la nombre.
        if destination == guard.server || destination == local {
            batch.push(payload);
        }
    }
    for payload in batch {
        if let Err(error) = guard.socket.send(&payload) {
            if error.kind() == ErrorKind::WouldBlock {
                break;
            }
            push_event(&mut guard, BridgeEvent::IoFault(format!("send {error}")));
            guard.stop = true;
            return Err("envío UDP falló".into());
        }
    }
    Ok(())
}

fn publish(guard: &mut Shared) {
    let hello_id = guard.hello_id.clone();
    while let Some(event) = guard.client.next_event() {
        let mapped = match event {
            ClientEvent::Authorized { .. } => BridgeEvent::Authorized,
            ClientEvent::Rejected(_) => BridgeEvent::Rejected,
            ClientEvent::Closed => BridgeEvent::Closed,
        };
        push_event(guard, mapped);
    }
    while let Some(message) = guard.client.recv_control() {
        if let ControlMessage::Capabilities(caps) = &message {
            // Lo que el servidor declara se guarda antes de mapear: de aquí
            // sale la negociación de códec, y el hello no se publica como ACK.
            guard.server_features = caps
                .capabilities
                .features
                .iter()
                .map(|feature| feature.as_str().to_string())
                .collect();
            guard.server_revision = caps.revision;
        }
        if let Some(mapped) = map_control(&hello_id, message) {
            push_event(guard, mapped);
        }
    }
    // El servidor ya respondió (o el perfil cambió): se anuncian las features
    // locales. Un fallo de cola se reintenta en el siguiente bombeo.
    if !guard.stop {
        let _ = guard.advertise();
    }
    // Retorno: los datagramas que el transporte ya decodificó pasan a su cola
    // acotada. Con el retorno apagado no se drena ni se encola nada.
    if guard.return_audio.enabled() {
        while let Some(payload) = guard.client.recv_audio_payload() {
            guard.return_audio.push(payload);
        }
    }
}

fn map_control(hello_id: &str, message: ControlMessage) -> Option<BridgeEvent> {
    match message {
        ControlMessage::Ack(ack) => Some(ack_event(ack)),
        ControlMessage::Error(error) => Some(error_event(error)),
        // La respuesta al hello es capabilities, no un ACK de configuración.
        ControlMessage::Capabilities(caps) if caps.request_id != hello_id => {
            Some(BridgeEvent::Ack {
                request_id: caps.request_id,
                revision: caps.revision,
                applied: false,
                requested_json: "null".into(),
                effective_json: "null".into(),
            })
        }
        _ => None,
    }
}

fn error_event(error: ErrorMsg) -> BridgeEvent {
    BridgeEvent::Error {
        request_id: error.request_id,
        code: error.code.as_str().to_string(),
        current_revision: error.current_revision,
    }
}

fn ack_event(ack: AckMsg) -> BridgeEvent {
    let requested_json = json_requested(&ack);
    let effective_json = json_effective(&ack.effective);
    BridgeEvent::Ack {
        request_id: ack.request_id,
        revision: ack.revision,
        applied: ack.applied,
        requested_json,
        effective_json,
    }
}

fn json_requested(ack: &AckMsg) -> String {
    match &ack.requested {
        Some(pair) => format!(
            "{{\"key\":\"{}\",\"value\":{}}}",
            pair.key.as_str(),
            json_value(&pair.value)
        ),
        None => "null".into(),
    }
}

fn json_effective(effective: &AckEffective) -> String {
    match effective {
        AckEffective::Pair(pair) => format!(
            "{{\"key\":\"{}\",\"value\":{}}}",
            pair.key.as_str(),
            json_value(&pair.value)
        ),
        AckEffective::Snapshot(_) => "{\"snapshot\":true}".into(),
    }
}

fn json_value(value: &ConfigValue) -> String {
    match value {
        ConfigValue::Bool(v) => v.to_string(),
        ConfigValue::Integer(v) => v.to_string(),
        ConfigValue::Number(v) => {
            if v.fract() == 0.0 {
                format!("{v:.1}")
            } else {
                v.to_string()
            }
        }
        ConfigValue::String(v) => format!("\"{}\"", v.replace('"', "")),
    }
}

fn push_event(guard: &mut Shared, event: BridgeEvent) {
    if guard.events.len() >= EVENT_CAP {
        guard.events.remove(0);
    }
    guard.events.push(event);
}

fn pull_terminal(events: &mut Vec<BridgeEvent>) -> Option<BridgeEvent> {
    let index = events.iter().position(|event| {
        matches!(
            event,
            BridgeEvent::Authorized
                | BridgeEvent::Rejected
                | BridgeEvent::Closed
                | BridgeEvent::IoFault(_)
        )
    })?;
    Some(events.remove(index))
}

fn show<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

fn request_id() -> String {
    use std::sync::atomic::AtomicU64;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("a{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Codifica y descarta: red de seguridad para que un mensaje de producto
/// que no pase el validador de `openrelay-proto` falle aquí y no en el socket.
#[allow(dead_code)]
pub(crate) fn encode_checked(message: &ControlMessage) -> Result<Vec<u8>, String> {
    encode_message(message).map_err(|e| format!("{e:?}"))
}
