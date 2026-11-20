//! Control compartido del daemon: una revisión, varias identidades.
//!
//! El canal es el de siempre: `SessionServer::recv_control` y
//! `send_control` sobre la sesión QUIC ya autorizada. Aquí no hay otro
//! protocolo ni otro puerto. La CA de test no entra en este módulo.
//!
//! Cada identidad trae su permiso y su caché de `request_id`. El estado y
//! la revisión son uno solo, el de [`SharedConfig`]. `config.write` no
//! concede micrófono ni audio de retorno: esas capacidades viven en el
//! transporte y este módulo no las toca.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use openrelay_proto::control::{
    AuthorizedPeer, ConfigKey, ConfigValue, ControlSession, EffectivePolicy, ErrorCode, Feature,
    Outcome, PeerRole, RemoteAction, SharedConfig, UnsupportedPolicy,
};
use openrelay_proto::wire::ControlMessage;
use openrelay_transport::identity::SpkiPin;
use openrelay_transport::quinn::{SessionId, SessionServer, TransportError};

/// Tope de identidades registradas. Pasado el tope, registrar otra falla
/// en cerrado en vez de crecer sin límite.
const MAX_IDENTITIES: usize = 64;

/// Cómo llega una llamada de control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOrigin {
    /// Administrador local (el CLI futuro entra por aquí).
    Local,
    /// Sesión QUIC ya autorizada.
    Remote,
}

/// Permiso y rol de una identidad. No incluye micrófono ni audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityGrant {
    /// Texto opaco que viaja en `capabilities.device_id`.
    pub device_id: String,
    /// `linux-cli` para el administrador local, `android` para el remoto.
    pub role: PeerRole,
    /// `config.read` y `config.write`, nada más.
    pub actions: Vec<RemoteAction>,
}

impl IdentityGrant {
    fn peer(&self) -> AuthorizedPeer {
        AuthorizedPeer::new(&self.device_id, self.role, self.actions.clone())
    }
}

/// Registro de identidades que pueden hablar control.
#[derive(Debug, Default)]
pub struct ControlRegistry {
    grants: HashMap<SpkiPin, IdentityGrant>,
}

impl ControlRegistry {
    /// Registro vacío: nadie puede leer ni escribir.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cuántas identidades hay registradas.
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// Si no hay ninguna.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Registra o reemplaza el permiso de una identidad.
    ///
    /// `pin` es el SPKI que el transporte ya autenticó. `grant` sólo puede
    /// llevar `config.read` y `config.write`: no hay acción de micrófono
    /// que registrar. Falla si el registro está lleno y el pin es nuevo.
    pub fn register(&mut self, pin: SpkiPin, grant: IdentityGrant) -> Result<(), ControlError> {
        if !self.grants.contains_key(&pin) && self.grants.len() >= MAX_IDENTITIES {
            return Err(ControlError::Full);
        }
        self.grants.insert(pin, grant);
        Ok(())
    }

    /// Quita una identidad. Las sesiones ya abiertas dejan de responder.
    pub fn revoke(&mut self, pin: &SpkiPin) -> bool {
        self.grants.remove(pin).is_some()
    }

    fn grant(&self, pin: &SpkiPin) -> Option<&IdentityGrant> {
        self.grants.get(pin)
    }
}

/// Error de este módulo. El detalle sensible no sale de aquí.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// La identidad no está registrada.
    Unknown,
    /// El registro llegó al tope.
    Full,
    /// El transporte rechazó el envío. El texto dice cuál.
    Transport(&'static str),
    /// El estado compartido no se pudo leer (cerrojo envenenado).
    State,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("identity not registered"),
            Self::Full => f.write_str("control registry full"),
            Self::Transport(detail) => write!(f, "transport rejected control: {detail}"),
            Self::State => f.write_str("control state unavailable"),
        }
    }
}

impl std::error::Error for ControlError {}

/// Política compartida por todas las sesiones de un `SharedControl`.
struct SharedPolicy(Arc<dyn EffectivePolicy>);

impl EffectivePolicy for SharedPolicy {
    fn effective(
        &self,
        key: &ConfigKey,
        requested: &ConfigValue,
    ) -> Result<ConfigValue, ErrorCode> {
        self.0.effective(key, requested)
    }
}

/// Una revisión para todo el daemon y una sesión de control por identidad.
pub struct SharedControl {
    state: SharedConfig,
    registry: ControlRegistry,
    policy: Arc<dyn EffectivePolicy>,
    sessions: HashMap<SpkiPin, ControlSession>,
    /// Features que cada identidad remota anunció. Sólo se llenan con un
    /// mensaje `capabilities` explícito; sin él no hay audio Opus ni retorno.
    peer_features: HashMap<SpkiPin, Vec<Feature>>,
    /// Administrador local. Su pin no existe en el transporte.
    local: Option<ControlSession>,
}

impl SharedControl {
    /// Control de producto: sin aplicador de hardware, todo `config.set` se
    /// responde `unsupported` sin mutar revisión ni valor.
    pub fn new(registry: ControlRegistry) -> Self {
        Self::with_policy(registry, Box::new(UnsupportedPolicy))
    }

    /// Control con una política de efecto explícita. Sólo para pruebas o para
    /// un aplicador real; el producto usa [`Self::new`].
    pub fn with_policy(registry: ControlRegistry, policy: Box<dyn EffectivePolicy>) -> Self {
        Self {
            state: SharedConfig::new(),
            registry,
            policy: Arc::from(policy),
            sessions: HashMap::new(),
            peer_features: HashMap::new(),
            local: None,
        }
    }

    /// Revisión que ven todas las identidades.
    pub fn revision(&self) -> u64 {
        self.state.revision()
    }

    /// ¿La identidad remota anunció `audio.opus-48k-mono`? Sin un mensaje
    /// `capabilities` en esta ejecución la respuesta es `false`.
    pub fn opus_negotiated(&self, pin: &SpkiPin) -> bool {
        self.peer_features(pin)
            .is_some_and(|features| features.contains(&Feature::AudioOpus48kMono))
    }

    /// Features anunciadas por una identidad remota en esta ejecución.
    ///
    /// `None` si esa identidad todavía no envió un mensaje `capabilities`.
    pub fn peer_features(&self, pin: &SpkiPin) -> Option<&[Feature]> {
        self.peer_features.get(pin).map(Vec::as_slice)
    }

    /// ¿La clave local `audio.return_enabled` vale `true`?
    ///
    /// Es una clave local del daemon, con default `false`. Si el estado no se
    /// puede leer, la respuesta es `false`: el retorno no se enciende solo.
    pub fn return_enabled(&self) -> bool {
        let Ok(key) = ConfigKey::new("audio.return_enabled") else {
            return false;
        };
        matches!(
            self.state.snapshot().get(&key),
            Some(ConfigValue::Bool(true))
        )
    }

    /// Is the effective `audio.mute` value `true`?
    ///
    /// The CLI or the app writes it through `config.set`; the audio path reads
    /// it every tick. If the state cannot be read the answer is `false`: a
    /// poisoned lock must not silence the virtual microphone.
    pub fn muted(&self) -> bool {
        let Ok(key) = ConfigKey::new("audio.mute") else {
            return false;
        };
        matches!(
            self.state.snapshot().get(&key),
            Some(ConfigValue::Bool(true))
        )
    }

    /// Ganancia efectiva en dB que el runtime aplica a las muestras del
    /// micrófono. `0.0` si el estado no se puede leer: el audio no se amplifica
    /// solo.
    pub fn gain_db(&self) -> f32 {
        let Ok(key) = ConfigKey::new("audio.gain_db") else {
            return 0.0;
        };
        match self.state.snapshot().get(&key) {
            Some(ConfigValue::Number(db)) if db.is_finite() => *db as f32,
            _ => 0.0,
        }
    }

    /// Registro con el que se construyó. Sirve para registrar después.
    pub fn registry_mut(&mut self) -> &mut ControlRegistry {
        // Un pin nuevo no tiene sesión; uno ya visto conserva la suya y
        // toma el permiso nuevo en la próxima llamada.
        self.sessions.clear();
        &mut self.registry
    }

    /// Abre la sesión del administrador local.
    ///
    /// Es la entrada que usará el CLI. No pasa por QUIC y no hereda ninguna
    /// capacidad de transporte: sólo las acciones del `grant`.
    pub fn open_local(&mut self, grant: IdentityGrant) -> Result<(), ControlError> {
        self.local = Some(ControlSession::sharing(
            grant.peer(),
            self.state.clone(),
            self.policy_box(),
        ));
        Ok(())
    }

    /// Aplica un mensaje ya decodificado como el administrador local.
    pub fn local_call(&mut self, message: &ControlMessage) -> Result<Outcome, ControlError> {
        let session = self.local.as_mut().ok_or(ControlError::Unknown)?;
        Ok(session.handle(message, now_ms()))
    }

    /// Atiende el control que ya llegó por las sesiones QUIC.
    ///
    /// `sessions` son las que el llamador vio autorizadas. Lee con
    /// `recv_control`, responde con `send_control` y no abre nada más. Una
    /// identidad sin registro no recibe respuesta. Un permiso que no alcanza
    /// produce el `error` `denied` del catálogo, sin mover la revisión.
    ///
    /// Un `outcome.close` cierra esa sesión con `close_session` y no se le
    /// vuelve a leer control. `QueueFull` y el resto de fallos de envío hacen
    /// lo mismo: se aísla ese peer y las demás sesiones siguen. El error no se
    /// traga para seguir encolando: si la cola no admite el frame, esa sesión
    /// deja de usarse. Un fallo global (`State`) sí sale de `poll`.
    pub fn poll(
        &mut self,
        server: &mut SessionServer,
        sessions: &[SessionId],
    ) -> Result<(), ControlError> {
        for session_id in sessions {
            self.poll_session(server, *session_id)?;
        }
        Ok(())
    }

    fn poll_session(
        &mut self,
        server: &mut SessionServer,
        session_id: SessionId,
    ) -> Result<(), ControlError> {
        let Some(pin) = server.identity(session_id) else {
            return Ok(());
        };
        loop {
            let Some(message) = server.recv_control(session_id) else {
                return Ok(());
            };
            if let ControlMessage::Capabilities(capabilities) = &message {
                self.peer_features
                    .insert(pin, capabilities.capabilities.features.clone());
            }
            let outcome = match self.session_for(&pin) {
                Some(session) => session.handle(&message, now_ms()),
                None => {
                    let response = denied(&message);
                    if let Err(error) = server.send_control(session_id, &response) {
                        return self.fail_peer(server, session_id, error);
                    }
                    continue;
                }
            };
            let must_close = outcome.close;
            if let Some(response) = outcome.response {
                if let Err(error) = server.send_control(session_id, &response) {
                    return self.fail_peer(server, session_id, error);
                }
            }
            for event in &outcome.events {
                if let Err(error) = server.send_control(session_id, event) {
                    return self.fail_peer(server, session_id, error);
                }
            }
            if must_close {
                server.close_session(session_id, Instant::now());
                return Ok(());
            }
        }
    }

    /// Aísla un peer cuyo envío de control falló y deja el daemon en pie.
    ///
    /// `State` no es un fallo de este peer: se propaga. El resto cierra la
    /// sesión (el transporte ya no entrega su control ni su audio) y `poll`
    /// sigue con las demás.
    fn fail_peer(
        &self,
        server: &mut SessionServer,
        session_id: SessionId,
        error: TransportError,
    ) -> Result<(), ControlError> {
        let mapped = map_transport(error);
        if matches!(mapped, ControlError::State) {
            return Err(mapped);
        }
        server.close_session(session_id, Instant::now());
        Ok(())
    }

    fn policy_box(&self) -> Box<dyn EffectivePolicy> {
        Box::new(SharedPolicy(Arc::clone(&self.policy)))
    }

    fn session_for(&mut self, pin: &SpkiPin) -> Option<&mut ControlSession> {
        if self.sessions.contains_key(pin) {
            return self.sessions.get_mut(pin);
        }
        let grant = self.registry.grant(pin)?.clone();
        let session = ControlSession::sharing(grant.peer(), self.state.clone(), self.policy_box());
        self.sessions.insert(*pin, session);
        self.sessions.get_mut(pin)
    }
}

fn denied(message: &ControlMessage) -> ControlMessage {
    let request_id = message.request_id().unwrap_or("unknown");
    match openrelay_proto::control::ErrorMsg::new(
        request_id,
        ErrorCode::Denied,
        "identity not registered",
    ) {
        Ok(error) => ControlMessage::Error(error),
        Err(_) => ControlMessage::Error(
            openrelay_proto::control::ErrorMsg::new(
                "unknown",
                ErrorCode::Internal,
                "denied response failed",
            )
            .expect("valid internal text"),
        ),
    }
}

fn map_transport(error: TransportError) -> ControlError {
    let detail = match error {
        TransportError::Unauthorized => return ControlError::Unknown,
        TransportError::Capability(cap) => cap,
        TransportError::Closed => "session closed",
        TransportError::Stream(_) => "control stream",
        TransportError::QueueFull => "control queue full",
        TransportError::Control(_) => "invalid control frame",
        _ => "transport",
    };
    ControlError::Transport(detail)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrelay_proto::wire::{decode_message, encode_frame, AckMsg};

    /// Política de producto mínima: deja pasar sólo `audio.return_enabled`.
    ///
    /// No es la del producto (hoy `UnsupportedPolicy`): fija el contrato que
    /// esa política tiene que cumplir para que `config set` encienda el
    /// retorno de verdad.
    struct ReturnEnabledPolicy;

    impl EffectivePolicy for ReturnEnabledPolicy {
        fn effective(
            &self,
            key: &ConfigKey,
            requested: &ConfigValue,
        ) -> Result<ConfigValue, ErrorCode> {
            if key.as_str() == "audio.return_enabled" {
                Ok(requested.clone())
            } else {
                Err(ErrorCode::Unsupported)
            }
        }
    }

    fn set_frame(request_id: &str, value: bool, revision: u64) -> Vec<u8> {
        let json = format!(
            r#"{{"schema_version":"openrelay.control/v1","type":"config.set","request_id":"{request_id}","expected_revision":{revision},"payload":{{"key":"audio.return_enabled","value":{value}}}}}"#
        );
        encode_frame(json.as_bytes()).expect("frame")
    }

    #[test]
    fn config_set_audio_return_enabled_applies_when_the_policy_allows_it() {
        let mut control =
            SharedControl::with_policy(ControlRegistry::new(), Box::new(ReturnEnabledPolicy));
        control
            .open_local(IdentityGrant {
                device_id: "openrelay-cli-local".into(),
                role: PeerRole::LinuxCli,
                actions: vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
            })
            .expect("local session");
        assert!(!control.return_enabled(), "apagado por defecto");

        let on = decode_message(&set_frame("r-1", true, 0)).expect("frame");
        let outcome = control.local_call(&on).expect("local call");
        assert!(
            matches!(outcome.response, Some(ControlMessage::Ack(AckMsg { .. }))),
            "el set se confirma"
        );
        assert_eq!(control.revision(), 1, "una revisión por set aplicado");
        assert!(control.return_enabled(), "el valor efectivo es el pedido");

        let off = decode_message(&set_frame("r-2", false, 1)).expect("frame");
        control.local_call(&off).expect("local call");
        assert!(!control.return_enabled(), "también se apaga");
    }
}
