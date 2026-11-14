//! Máquina de control del daemon: configuración, revisión, conflicto y ACK.
//!
//! Modela el worker único de control de `docs/protocol.md` §6 y
//! `docs/config-schema.md`: la revisión empieza en `0` por proceso, sólo un
//! `config.set` aplicado la incrementa exactamente +1, y cada sesión guarda
//! hasta 64 respuestas (`ack` o `error`) durante 60 s para ser idempotente.
//!
//! La autenticación y la ACL no se implementan aquí: [`AuthorizedPeer`] es la
//! frontera que F2 entregará ya resuelta. Esta crate no expone ninguna acción
//! remota de micrófono, permisos ni claves locales.

use std::collections::VecDeque;

use crate::wire::{
    AckMsg, CapabilitiesMsg, ConfigGetMsg, ConfigSetMsg, ControlMessage, EventBody, EventMsg,
    HelloMsg, WireError, MAX_REVISION, SCHEMA_VERSION,
};

/// `request_id` máximo en caracteres.
pub const MAX_REQUEST_ID_CHARS: usize = 64;
/// Claves declaradas máximas en `capabilities`.
pub const MAX_CONFIG_KEYS_DECLARED: usize = 32;
/// Claves máximas en un snapshot.
pub const CONFIG_SNAPSHOT_MAX_KEYS: usize = 32;
/// Caracteres máximos de un valor string.
pub const MAX_CONFIG_STRING_CHARS: usize = 1024;
/// Caracteres máximos del `message` de un error.
pub const MAX_MESSAGE_CHARS: usize = 200;
/// Entradas de la caché de idempotencia por sesión.
pub const IDEMPOTENCY_CAPACITY: usize = 64;
/// TTL de la caché de idempotencia.
pub const IDEMPOTENCY_TTL_MS: u64 = 60_000;

// ------------------------------------------------------------------ errores --

/// Códigos del catálogo `contracts/errors.md`, sin códigos nuevos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Mensaje, campo, tipo, valor o longitud inválidos.
    Invalid,
    /// Versión de esquema o protocolo desconocida.
    VersionUnsupported,
    /// Mismo `request_id` con payload distinto.
    RequestIdReuse,
    /// `expected_revision` no coincide.
    Conflict,
    /// Clave local enviada por el canal remoto.
    LocalKey,
    /// Operación no permitida por ACL o clave readonly.
    Denied,
    /// Operación o tipo no soportado en v1.
    Unsupported,
    /// Recurso ocupado.
    Busy,
    /// Identidad ausente, inválida o revocada.
    Unauthorized,
    /// Frame o mensaje por encima del límite.
    TooLarge,
    /// Clave, sesión o identidad inexistente.
    NotFound,
    /// Fallo interno sin detalle sensible.
    Internal,
}

impl ErrorCode {
    /// Nombre exacto del catálogo.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Invalid => "invalid",
            ErrorCode::VersionUnsupported => "version_unsupported",
            ErrorCode::RequestIdReuse => "request_id_reuse",
            ErrorCode::Conflict => "conflict",
            ErrorCode::LocalKey => "local_key",
            ErrorCode::Denied => "denied",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Busy => "busy",
            ErrorCode::Unauthorized => "unauthorized",
            ErrorCode::TooLarge => "too_large",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Internal => "internal",
        }
    }

    /// Reconoce un código del catálogo.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "invalid" => Some(ErrorCode::Invalid),
            "version_unsupported" => Some(ErrorCode::VersionUnsupported),
            "request_id_reuse" => Some(ErrorCode::RequestIdReuse),
            "conflict" => Some(ErrorCode::Conflict),
            "local_key" => Some(ErrorCode::LocalKey),
            "denied" => Some(ErrorCode::Denied),
            "unsupported" => Some(ErrorCode::Unsupported),
            "busy" => Some(ErrorCode::Busy),
            "unauthorized" => Some(ErrorCode::Unauthorized),
            "too_large" => Some(ErrorCode::TooLarge),
            "not_found" => Some(ErrorCode::NotFound),
            "internal" => Some(ErrorCode::Internal),
            _ => None,
        }
    }

    /// `retryable` normativo de `contracts/errors.md` §2.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::Conflict | ErrorCode::Busy | ErrorCode::Internal
        )
    }

    /// Si el contrato exige cerrar la conexión al emitirlo.
    pub fn closes_connection(self) -> bool {
        matches!(
            self,
            ErrorCode::Unauthorized | ErrorCode::VersionUnsupported | ErrorCode::TooLarge
        )
    }
}

/// Mensaje `error` validado.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorMsg {
    /// `request_id` copiado del mensaje que causó el error.
    pub request_id: String,
    /// Código del catálogo.
    pub code: ErrorCode,
    /// Texto corto sin secretos.
    pub message: String,
    /// Derivado del código; nunca se deduce del texto.
    pub retryable: bool,
    /// Sólo para `conflict`.
    pub current_revision: Option<u64>,
}

impl ErrorMsg {
    /// Construye un error con `retryable` derivado del código.
    pub fn new(request_id: &str, code: ErrorCode, message: &str) -> Result<Self, WireError> {
        if !valid_request_id(request_id)
            || message.is_empty()
            || message.chars().count() > MAX_MESSAGE_CHARS
        {
            return Err(WireError::invalid(None));
        }
        Ok(Self {
            request_id: request_id.to_owned(),
            code,
            message: message.to_owned(),
            retryable: code.retryable(),
            current_revision: None,
        })
    }

    /// Añade `current_revision`; sólo el código `conflict` lo admite.
    pub fn with_current_revision(mut self, revision: u64) -> Result<Self, WireError> {
        if self.code != ErrorCode::Conflict || revision > MAX_REVISION {
            return Err(WireError::invalid(None));
        }
        self.current_revision = Some(revision);
        Ok(self)
    }
}

fn valid_request_id(request_id: &str) -> bool {
    let count = request_id.chars().count();
    (1..=MAX_REQUEST_ID_CHARS).contains(&count)
        && request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// -------------------------------------------------------------------- claves --

/// Alcance de una clave de configuración.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    /// Se comparte con el canal remoto.
    Shared,
    /// Preferencia local del dispositivo.
    Local,
    /// Informativa y no escribible.
    Readonly,
}

impl ConfigScope {
    /// Nombre del contrato.
    pub fn as_str(self) -> &'static str {
        match self {
            ConfigScope::Shared => "shared",
            ConfigScope::Local => "local",
            ConfigScope::Readonly => "readonly",
        }
    }
}

/// Tipo declarado de una clave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    /// Booleano.
    Boolean,
    /// Entero.
    Integer,
    /// Número.
    Number,
    /// String UTF-8.
    String,
}

impl ValueType {
    /// Nombre del contrato.
    pub fn as_str(self) -> &'static str {
        match self {
            ValueType::Boolean => "boolean",
            ValueType::Integer => "integer",
            ValueType::Number => "number",
            ValueType::String => "string",
        }
    }
}

/// Clave de configuración validada contra el patrón del contrato.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConfigKey(String);

impl ConfigKey {
    /// Valida `^[a-z][a-z0-9_]*(\.[a-z0-9_]+){0,3}$` y 1..=64 caracteres.
    pub fn new(name: &str) -> Result<Self, ErrorCode> {
        if name.is_empty() || name.chars().count() > 64 {
            return Err(ErrorCode::Invalid);
        }
        let mut segments = name.split('.');
        let first = segments.next().ok_or(ErrorCode::Invalid)?;
        if first.is_empty()
            || !first.starts_with(|c: char| c.is_ascii_lowercase())
            || !first
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(ErrorCode::Invalid);
        }
        let mut count = 0;
        for segment in segments {
            count += 1;
            if count > 3
                || segment.is_empty()
                || !segment
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(ErrorCode::Invalid);
            }
        }
        Ok(Self(name.to_owned()))
    }

    /// Texto de la clave.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConfigKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Valor tipado de configuración.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigValue {
    /// Booleano.
    Bool(bool),
    /// Entero exacto.
    Integer(i64),
    /// Número finito.
    Number(f64),
    /// String UTF-8 acotado.
    String(String),
}

/// Declaración estática de una clave v1.
#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    /// Nombre de la clave.
    pub name: &'static str,
    /// Alcance.
    pub scope: ConfigScope,
    /// Tipo.
    pub value_type: ValueType,
    /// Mínimo inclusivo, si aplica.
    pub minimum: Option<f64>,
    /// Máximo inclusivo, si aplica.
    pub maximum: Option<f64>,
    /// Paso obligatorio, si aplica (sólo `audio.gain_db`).
    pub step: Option<f64>,
    /// Longitud máxima en caracteres para strings.
    pub max_chars: Option<usize>,
}

/// Catálogo completo de claves v1, en orden de declaración.
pub const CONFIG_KEY_SPECS: [KeySpec; 9] = [
    KeySpec {
        name: "audio.sample_rate_hz",
        scope: ConfigScope::Shared,
        value_type: ValueType::Integer,
        minimum: Some(48_000.0),
        maximum: Some(48_000.0),
        step: None,
        max_chars: None,
    },
    KeySpec {
        name: "audio.frame_duration_ms",
        scope: ConfigScope::Shared,
        value_type: ValueType::Integer,
        minimum: Some(5.0),
        maximum: Some(5.0),
        step: None,
        max_chars: None,
    },
    KeySpec {
        name: "audio.gain_db",
        scope: ConfigScope::Shared,
        value_type: ValueType::Number,
        minimum: Some(-24.0),
        maximum: Some(24.0),
        step: Some(0.5),
        max_chars: None,
    },
    KeySpec {
        name: "audio.mute",
        scope: ConfigScope::Shared,
        value_type: ValueType::Boolean,
        minimum: None,
        maximum: None,
        step: None,
        max_chars: None,
    },
    KeySpec {
        name: "audio.jitter_target_ms",
        scope: ConfigScope::Shared,
        value_type: ValueType::Integer,
        minimum: Some(5.0),
        maximum: Some(40.0),
        step: None,
        max_chars: None,
    },
    KeySpec {
        name: "ui.theme",
        scope: ConfigScope::Local,
        value_type: ValueType::String,
        minimum: None,
        maximum: None,
        step: None,
        max_chars: Some(1024),
    },
    KeySpec {
        name: "ui.text_scale",
        scope: ConfigScope::Local,
        value_type: ValueType::Number,
        minimum: Some(1.0),
        maximum: Some(2.0),
        step: None,
        max_chars: None,
    },
    KeySpec {
        name: "device.local_volume",
        scope: ConfigScope::Local,
        value_type: ValueType::Number,
        minimum: Some(0.0),
        maximum: Some(1.0),
        step: None,
        max_chars: None,
    },
    KeySpec {
        name: "device.model",
        scope: ConfigScope::Readonly,
        value_type: ValueType::String,
        minimum: None,
        maximum: None,
        step: None,
        max_chars: Some(64),
    },
];

/// Catálogo de claves v1.
pub fn config_keys() -> &'static [KeySpec] {
    &CONFIG_KEY_SPECS
}

/// Busca una clave declarada.
pub fn lookup_key(name: &str) -> Option<&'static KeySpec> {
    CONFIG_KEY_SPECS.iter().find(|spec| spec.name == name)
}

fn in_range(spec: &KeySpec, value: f64) -> bool {
    spec.minimum.map_or(true, |minimum| value >= minimum)
        && spec.maximum.map_or(true, |maximum| value <= maximum)
}

fn respects_step(spec: &KeySpec, value: f64) -> bool {
    match spec.step {
        Some(step) => (value / step).fract() == 0.0,
        None => true,
    }
}

/// Valida un valor contra el tipo, rango y paso de su clave.
pub fn validate_value(spec: &KeySpec, value: &ConfigValue) -> Result<(), ErrorCode> {
    match (spec.value_type, value) {
        (ValueType::Boolean, ConfigValue::Bool(_)) => Ok(()),
        (ValueType::Integer, ConfigValue::Integer(number)) => {
            if in_range(spec, *number as f64) {
                Ok(())
            } else {
                Err(ErrorCode::Invalid)
            }
        }
        (ValueType::Integer, ConfigValue::Number(number)) => {
            if number.fract() == 0.0 && in_range(spec, *number) {
                Ok(())
            } else {
                Err(ErrorCode::Invalid)
            }
        }
        (ValueType::Number, ConfigValue::Integer(number)) => {
            if in_range(spec, *number as f64) {
                Ok(())
            } else {
                Err(ErrorCode::Invalid)
            }
        }
        (ValueType::Number, ConfigValue::Number(number)) => {
            if number.is_finite() && in_range(spec, *number) && respects_step(spec, *number) {
                Ok(())
            } else {
                Err(ErrorCode::Invalid)
            }
        }
        (ValueType::String, ConfigValue::String(text)) => {
            let limit = spec.max_chars.unwrap_or(MAX_CONFIG_STRING_CHARS);
            if text.chars().count() <= limit {
                Ok(())
            } else {
                Err(ErrorCode::Invalid)
            }
        }
        _ => Err(ErrorCode::Invalid),
    }
}

// ------------------------------------------------------------------ estados --

/// Par `{key, value}` de un ACK o evento.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigPair {
    /// Clave.
    pub key: ConfigKey,
    /// Valor.
    pub value: ConfigValue,
}

/// Snapshot `clave → valor` de la configuración efectiva.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    entries: Vec<(ConfigKey, ConfigValue)>,
}

impl Snapshot {
    /// Snapshot vacío (sólo para construir o decodificar).
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Inserta o reemplaza un par.
    pub fn insert(&mut self, key: ConfigKey, value: ConfigValue) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(existing, _)| existing == &key)
        {
            entry.1 = value;
        } else {
            self.entries.push((key, value));
        }
    }

    /// Número de claves.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Si no hay claves.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Valor efectivo de una clave.
    pub fn get(&self, key: &ConfigKey) -> Option<&ConfigValue> {
        self.entries
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    /// Pares en orden de declaración.
    pub fn iter(&self) -> impl Iterator<Item = (&ConfigKey, &ConfigValue)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// Estado de configuración del daemon con su revisión.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigState {
    revision: u64,
    values: Vec<(ConfigKey, ConfigValue)>,
}

impl ConfigState {
    /// Estado con los defaults de `docs/config-schema.md` y revisión 0.
    pub fn new() -> Self {
        let values = CONFIG_KEY_SPECS
            .iter()
            .map(|spec| {
                (
                    ConfigKey::new(spec.name).expect("clave estática del catálogo"),
                    default_value(spec.name),
                )
            })
            .collect();
        Self {
            revision: 0,
            values,
        }
    }

    /// Estado con una revisión dada (restauración o pruebas de borde).
    pub fn with_revision(revision: u64) -> Self {
        let mut state = Self::new();
        state.revision = revision.min(MAX_REVISION);
        state
    }

    /// Revisión actual.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Valor efectivo de una clave declarada.
    pub fn get(&self, key: &ConfigKey) -> Option<&ConfigValue> {
        self.values
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, value)| value)
    }

    /// Snapshot completo en orden de declaración.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            entries: self.values.clone(),
        }
    }

    fn apply(&mut self, key: &ConfigKey, value: ConfigValue, revision: u64) {
        if let Some(entry) = self.values.iter_mut().find(|(existing, _)| existing == key) {
            entry.1 = value;
        }
        self.revision = revision;
    }
}

impl Default for ConfigState {
    fn default() -> Self {
        Self::new()
    }
}

fn default_value(key: &str) -> ConfigValue {
    match key {
        "audio.sample_rate_hz" => ConfigValue::Integer(48_000),
        "audio.frame_duration_ms" => ConfigValue::Integer(5),
        "audio.gain_db" => ConfigValue::Number(0.0),
        "audio.mute" => ConfigValue::Bool(false),
        "audio.jitter_target_ms" => ConfigValue::Integer(10),
        "ui.theme" => ConfigValue::String("system".into()),
        "ui.text_scale" => ConfigValue::Number(1.0),
        "device.local_volume" => ConfigValue::Number(1.0),
        "device.model" => ConfigValue::String("unknown".into()),
        _ => ConfigValue::Bool(false),
    }
}

// -------------------------------------------------------------------- actor --

/// Rol del extremo, tal como lo declara `hello` y `capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// Cliente Android (canal remoto).
    Android,
    /// CLI local.
    LinuxCli,
    /// Daemon Linux.
    LinuxDaemon,
}

impl PeerRole {
    /// Nombre del contrato.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerRole::Android => "android",
            PeerRole::LinuxCli => "linux-cli",
            PeerRole::LinuxDaemon => "linux-daemon",
        }
    }

    /// Si el rol llega por el canal remoto.
    pub fn is_remote(self) -> bool {
        matches!(self, PeerRole::Android)
    }
}

/// Acciones que un extremo admite por canal remoto. No existe ninguna acción
/// de micrófono, permisos ni claves locales.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAction {
    /// Leer configuración.
    ConfigRead,
    /// Escribir configuración compartida.
    ConfigWrite,
}

impl RemoteAction {
    /// Nombre del contrato.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteAction::ConfigRead => "config.read",
            RemoteAction::ConfigWrite => "config.write",
        }
    }
}

/// Clasifica una acción remota; micrófono y permisos no existen en v1.
pub fn classify_remote_action(name: &str) -> Result<RemoteAction, ErrorCode> {
    match name {
        "config.read" => Ok(RemoteAction::ConfigRead),
        "config.write" => Ok(RemoteAction::ConfigWrite),
        _ => Err(ErrorCode::Unsupported),
    }
}

/// Frontera de autenticación resuelta por F2. No autentica: declara.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorizedPeer {
    device_id: String,
    role: PeerRole,
    actions: Vec<RemoteAction>,
}

impl AuthorizedPeer {
    /// Construye el contexto ya autorizado por la capa de sesión.
    pub fn new(device_id: &str, role: PeerRole, actions: Vec<RemoteAction>) -> Self {
        Self {
            device_id: device_id.to_owned(),
            role,
            actions,
        }
    }

    /// Identificador opaco del dispositivo.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Rol declarado.
    pub fn role(&self) -> PeerRole {
        self.role
    }

    /// Acciones concedidas a esta sesión.
    pub fn actions(&self) -> &[RemoteAction] {
        &self.actions
    }

    /// Si la acción fue concedida.
    pub fn allows(&self, action: RemoteAction) -> bool {
        self.actions.contains(&action)
    }
}

/// Política tipada que puede limitar el valor efectivo (dispositivo u SO).
pub trait EffectivePolicy {
    /// Devuelve el efectivo para `requested`; la máquina lo revalida.
    fn effective(&self, key: &ConfigKey, requested: &ConfigValue) -> ConfigValue;
}

/// Política sin límites: el efectivo es el solicitado ya validado.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityPolicy;

impl EffectivePolicy for IdentityPolicy {
    fn effective(&self, _key: &ConfigKey, requested: &ConfigValue) -> ConfigValue {
        requested.clone()
    }
}

/// Payload de `capabilities` (esquema `arl.capabilities/v1`).
#[derive(Debug, Clone, PartialEq)]
pub struct Capabilities {
    /// Identificador opaco del dispositivo.
    pub device_id: String,
    /// Rol.
    pub role: PeerRole,
    /// Features soportadas.
    pub features: Vec<Feature>,
    /// Acciones remotas aceptadas.
    pub remote_actions: Vec<RemoteAction>,
    /// Límites constantes del contrato.
    pub limits: Limits,
    /// Claves declaradas.
    pub config_keys: Vec<ConfigKeyDeclaration>,
}

/// Límites constantes declarados en `capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// 16384.
    pub max_control_payload_bytes: u64,
    /// 1200.
    pub max_audio_datagram_bytes: u64,
    /// 20.
    pub max_reorder_ms: u64,
    /// 32.
    pub max_config_keys: u64,
    /// 64.
    pub max_request_id_len: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_control_payload_bytes: 16_384,
            max_audio_datagram_bytes: 1_200,
            max_reorder_ms: 20,
            max_config_keys: 32,
            max_request_id_len: 64,
        }
    }
}

/// Feature declarada en `capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// `control.v1`.
    ControlV1,
    /// `audio.pcm16-48k-mono`.
    AudioPcm16_48kMono,
    /// `enrollment.v1`.
    EnrollmentV1,
    /// `session.v1`.
    SessionV1,
}

impl Feature {
    /// Nombre del contrato.
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::ControlV1 => "control.v1",
            Feature::AudioPcm16_48kMono => "audio.pcm16-48k-mono",
            Feature::EnrollmentV1 => "enrollment.v1",
            Feature::SessionV1 => "session.v1",
        }
    }
}

/// Declaración de clave dentro de `capabilities`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigKeyDeclaration {
    /// Clave.
    pub key: ConfigKey,
    /// Alcance.
    pub scope: ConfigScope,
    /// Tipo.
    pub value_type: ValueType,
    /// Mínimo, si aplica.
    pub minimum: Option<f64>,
    /// Máximo, si aplica.
    pub maximum: Option<f64>,
}

impl Capabilities {
    /// Capacidades completas del catálogo v1 para un extremo autorizado.
    pub fn from_peer(device_id: &str, role: PeerRole, remote_actions: Vec<RemoteAction>) -> Self {
        let config_keys = CONFIG_KEY_SPECS
            .iter()
            .map(|spec| ConfigKeyDeclaration {
                key: ConfigKey::new(spec.name).expect("clave estática del catálogo"),
                scope: spec.scope,
                value_type: spec.value_type,
                minimum: spec.minimum,
                maximum: spec.maximum,
            })
            .collect();
        Self {
            device_id: device_id.to_owned(),
            role,
            features: vec![
                Feature::ControlV1,
                Feature::AudioPcm16_48kMono,
                Feature::SessionV1,
            ],
            remote_actions,
            limits: Limits::default(),
            config_keys,
        }
    }
}

/// Estado de sesión de un evento `session.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Sesión autorizada.
    Active,
    /// Sesión cerrada.
    Closed,
}

impl SessionState {
    /// Nombre del contrato.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Active => "active",
            SessionState::Closed => "closed",
        }
    }
}

// -------------------------------------------------------------------- salida --

/// Resultado de aplicar un mensaje: respuesta, eventos y cierre.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// Respuesta directa (`ack`, `error`, `capabilities`, `pong`), si la hay.
    pub response: Option<ControlMessage>,
    /// Eventos emitidos por el daemon (sólo `config.changed` al aplicar).
    pub events: Vec<ControlMessage>,
    /// Si el contrato exige cerrar la conexión.
    pub close: bool,
}

impl Outcome {
    fn reply(response: ControlMessage) -> Self {
        Self {
            response: Some(response),
            events: Vec::new(),
            close: false,
        }
    }

    fn nothing() -> Self {
        Self {
            response: None,
            events: Vec::new(),
            close: false,
        }
    }
}

// -------------------------------------------------------------- idempotencia --

#[derive(Debug, Clone, PartialEq)]
enum CanonicalOp {
    Get {
        key: Option<ConfigKey>,
    },
    Set {
        expected_revision: u64,
        key: ConfigKey,
        value: ConfigValue,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum CanonicalValue {
    Bool(bool),
    Integer(i64),
    Number(f64),
    String(String),
}

impl CanonicalValue {
    fn from(value: &ConfigValue) -> Self {
        match value {
            ConfigValue::Bool(value) => CanonicalValue::Bool(*value),
            ConfigValue::Integer(value) => CanonicalValue::Integer(*value),
            ConfigValue::Number(value) => {
                CanonicalValue::Number(if *value == 0.0 { 0.0 } else { *value })
            }
            ConfigValue::String(value) => CanonicalValue::String(value.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum CanonicalOpKey {
    Get {
        key: Option<ConfigKey>,
    },
    Set {
        key: ConfigKey,
        value: CanonicalValue,
    },
}

impl CanonicalOp {
    fn key(&self) -> CanonicalOpKey {
        match self {
            CanonicalOp::Get { key } => CanonicalOpKey::Get { key: key.clone() },
            CanonicalOp::Set { key, value, .. } => CanonicalOpKey::Set {
                key: key.clone(),
                value: CanonicalValue::from(value),
            },
        }
    }
}

#[derive(Debug)]
struct CacheEntry {
    request_id: String,
    op: CanonicalOpKey,
    stored_at: u64,
    response: ControlMessage,
}

#[derive(Debug, Default)]
struct IdempotencyCache {
    entries: VecDeque<CacheEntry>,
}

enum CacheLookup {
    Miss,
    Replay(ControlMessage),
    Reuse,
}

impl IdempotencyCache {
    fn purge_expired(&mut self, now_ms: u64) {
        self.entries
            .retain(|entry| now_ms.saturating_sub(entry.stored_at) < IDEMPOTENCY_TTL_MS);
    }

    fn lookup(&mut self, request_id: &str, op: &CanonicalOp, now_ms: u64) -> CacheLookup {
        self.purge_expired(now_ms);
        let found = self
            .entries
            .iter()
            .position(|entry| entry.request_id == request_id);
        match found {
            Some(index) => {
                if self.entries[index].op == op.key() {
                    CacheLookup::Replay(self.entries[index].response.clone())
                } else {
                    CacheLookup::Reuse
                }
            }
            None => CacheLookup::Miss,
        }
    }

    fn store(
        &mut self,
        request_id: &str,
        op: &CanonicalOp,
        response: &ControlMessage,
        now_ms: u64,
    ) {
        self.purge_expired(now_ms);
        if self.entries.len() >= IDEMPOTENCY_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(CacheEntry {
            request_id: request_id.to_owned(),
            op: op.key(),
            stored_at: now_ms,
            response: response.clone(),
        });
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

// -------------------------------------------------------------------- sesión --

/// Worker de control de una sesión autorizada.
pub struct ControlSession {
    peer: AuthorizedPeer,
    state: ConfigState,
    policy: Box<dyn EffectivePolicy>,
    cache: IdempotencyCache,
}

impl ControlSession {
    /// Crea la sesión con su estado inicial y política de efectivo.
    pub fn new(peer: AuthorizedPeer, state: ConfigState, policy: Box<dyn EffectivePolicy>) -> Self {
        Self {
            peer,
            state,
            policy,
            cache: IdempotencyCache::default(),
        }
    }

    /// Revisión actual del daemon.
    pub fn revision(&self) -> u64 {
        self.state.revision()
    }

    /// Snapshot efectivo actual.
    pub fn snapshot(&self) -> Snapshot {
        self.state.snapshot()
    }

    /// Entradas vivas de la caché de idempotencia (diagnóstico y pruebas).
    pub fn cached_responses(&self) -> usize {
        self.cache.len()
    }

    /// Aplica un mensaje ya validado en el orden de un worker único.
    pub fn handle(&mut self, message: &ControlMessage, now_ms: u64) -> Outcome {
        match message {
            ControlMessage::Hello(hello) => self.handle_hello(hello),
            ControlMessage::Capabilities(_) => Outcome::nothing(),
            ControlMessage::Ping { request_id } => Outcome::reply(ControlMessage::Pong {
                request_id: request_id.clone(),
            }),
            ControlMessage::Pong { .. } => Outcome::nothing(),
            ControlMessage::Error(_) => Outcome::nothing(),
            ControlMessage::Ack(ack) => Outcome::reply(error(
                &ack.request_id,
                ErrorCode::Unsupported,
                "ack is daemon to client",
            )),
            ControlMessage::Event(_) => Outcome::nothing(),
            ControlMessage::ConfigGet(get) => self.handle_get(get, now_ms),
            ControlMessage::ConfigSet(set) => self.handle_set(set, now_ms),
        }
    }

    /// Decodifica un frame y aplica el mensaje; los rechazos de wire siguen el
    /// catálogo de errores y cierran sólo cuando el contrato lo exige.
    pub fn handle_frame(&mut self, frame: &[u8], now_ms: u64) -> Outcome {
        match crate::wire::decode_message(frame) {
            Ok(message) => self.handle(&message, now_ms),
            Err(error) => Outcome {
                response: error.to_error_message(),
                events: Vec::new(),
                close: error.closes_connection(),
            },
        }
    }

    fn handle_hello(&mut self, hello: &HelloMsg) -> Outcome {
        if hello.protocol_version != SCHEMA_VERSION {
            let mut outcome = Outcome::reply(error(
                &hello.request_id,
                ErrorCode::VersionUnsupported,
                "version unsupported",
            ));
            outcome.close = true;
            return outcome;
        }
        let capabilities = Capabilities::from_peer(
            self.peer.device_id(),
            self.peer.role(),
            self.peer.actions().to_vec(),
        );
        Outcome::reply(ControlMessage::Capabilities(CapabilitiesMsg {
            request_id: hello.request_id.clone(),
            revision: self.state.revision(),
            capabilities,
        }))
    }

    fn handle_get(&mut self, get: &ConfigGetMsg, now_ms: u64) -> Outcome {
        if !self.peer.allows(RemoteAction::ConfigRead) {
            return Outcome::reply(error(
                &get.request_id,
                ErrorCode::Denied,
                "config.read not granted",
            ));
        }
        let op = CanonicalOp::Get {
            key: get.key.clone(),
        };
        match self.cache.lookup(&get.request_id, &op, now_ms) {
            CacheLookup::Replay(response) => return Outcome::reply(response),
            CacheLookup::Reuse => {
                return Outcome::reply(error(
                    &get.request_id,
                    ErrorCode::RequestIdReuse,
                    "request id reused with a different payload",
                ))
            }
            CacheLookup::Miss => {}
        }
        let response = match &get.key {
            None => ControlMessage::Ack(AckMsg {
                request_id: get.request_id.clone(),
                revision: self.state.revision(),
                applied: true,
                requested: None,
                effective: crate::wire::AckEffective::Snapshot(self.state.snapshot()),
            }),
            Some(key) => match lookup_key(key.as_str()) {
                None => error(&get.request_id, ErrorCode::NotFound, "config key not found"),
                Some(spec) if spec.scope == ConfigScope::Local && self.peer.role().is_remote() => {
                    error(&get.request_id, ErrorCode::LocalKey, "config key is local")
                }
                Some(_) => match self.state.get(key).cloned() {
                    Some(value) => ControlMessage::Ack(AckMsg {
                        request_id: get.request_id.clone(),
                        revision: self.state.revision(),
                        applied: true,
                        requested: None,
                        effective: crate::wire::AckEffective::Pair(ConfigPair {
                            key: key.clone(),
                            value,
                        }),
                    }),
                    None => error(
                        &get.request_id,
                        ErrorCode::Internal,
                        "config state incomplete",
                    ),
                },
            },
        };
        self.cache.store(&get.request_id, &op, &response, now_ms);
        Outcome::reply(response)
    }

    fn handle_set(&mut self, set: &ConfigSetMsg, now_ms: u64) -> Outcome {
        if !self.peer.allows(RemoteAction::ConfigWrite) {
            return Outcome::reply(error(
                &set.request_id,
                ErrorCode::Denied,
                "config.write not granted",
            ));
        }
        let op = CanonicalOp::Set {
            expected_revision: set.expected_revision,
            key: set.key.clone(),
            value: set.value.clone(),
        };
        match self.cache.lookup(&set.request_id, &op, now_ms) {
            CacheLookup::Replay(response) => return Outcome::reply(response),
            CacheLookup::Reuse => {
                return Outcome::reply(error(
                    &set.request_id,
                    ErrorCode::RequestIdReuse,
                    "request id reused with a different payload",
                ))
            }
            CacheLookup::Miss => {}
        }
        if set.expected_revision != self.state.revision() {
            let conflict = ErrorMsg::new(&set.request_id, ErrorCode::Conflict, "revision mismatch")
                .and_then(|error| error.with_current_revision(self.state.revision()))
                .map(ControlMessage::Error)
                .unwrap_or_else(|_| {
                    error(
                        &set.request_id,
                        ErrorCode::Internal,
                        "conflict response failed",
                    )
                });
            // Un conflicto es reintentable y depende de la revisión vigente, no
            // del payload: no se cachea para no bloquear el reintento corregido.
            return Outcome::reply(conflict);
        }
        let spec = match lookup_key(set.key.as_str()) {
            Some(spec) => spec,
            None => {
                let response = error(&set.request_id, ErrorCode::Invalid, "config key invalid");
                self.cache.store(&set.request_id, &op, &response, now_ms);
                return Outcome::reply(response);
            }
        };
        match spec.scope {
            ConfigScope::Local => {
                let response = error(&set.request_id, ErrorCode::LocalKey, "config key is local");
                self.cache.store(&set.request_id, &op, &response, now_ms);
                return Outcome::reply(response);
            }
            ConfigScope::Readonly => {
                let response = error(&set.request_id, ErrorCode::Denied, "config key is readonly");
                self.cache.store(&set.request_id, &op, &response, now_ms);
                return Outcome::reply(response);
            }
            ConfigScope::Shared => {}
        }
        if validate_value(spec, &set.value).is_err() {
            let response = error(&set.request_id, ErrorCode::Invalid, "config value invalid");
            self.cache.store(&set.request_id, &op, &response, now_ms);
            return Outcome::reply(response);
        }
        let effective = self.policy.effective(&set.key, &set.value);
        if validate_value(spec, &effective).is_err() {
            let response = error(
                &set.request_id,
                ErrorCode::Internal,
                "effective policy returned an invalid value",
            );
            self.cache.store(&set.request_id, &op, &response, now_ms);
            return Outcome::reply(response);
        }
        let current = self.state.get(&set.key).cloned();
        if current.as_ref() == Some(&effective) {
            let response = ControlMessage::Ack(AckMsg {
                request_id: set.request_id.clone(),
                revision: self.state.revision(),
                applied: false,
                requested: Some(ConfigPair {
                    key: set.key.clone(),
                    value: set.value.clone(),
                }),
                effective: crate::wire::AckEffective::Pair(ConfigPair {
                    key: set.key.clone(),
                    value: effective,
                }),
            });
            self.cache.store(&set.request_id, &op, &response, now_ms);
            return Outcome::reply(response);
        }
        let next_revision = match self.state.revision().checked_add(1) {
            Some(revision) if revision <= MAX_REVISION => revision,
            _ => {
                let response = error(&set.request_id, ErrorCode::Internal, "revision exhausted");
                self.cache.store(&set.request_id, &op, &response, now_ms);
                return Outcome::reply(response);
            }
        };
        self.state.apply(&set.key, effective.clone(), next_revision);
        let response = ControlMessage::Ack(AckMsg {
            request_id: set.request_id.clone(),
            revision: next_revision,
            applied: true,
            requested: Some(ConfigPair {
                key: set.key.clone(),
                value: set.value.clone(),
            }),
            effective: crate::wire::AckEffective::Pair(ConfigPair {
                key: set.key.clone(),
                value: effective.clone(),
            }),
        });
        self.cache.store(&set.request_id, &op, &response, now_ms);
        let event = ControlMessage::Event(EventMsg {
            revision: next_revision,
            event: EventBody::ConfigChanged {
                key: set.key.clone(),
                requested: set.value.clone(),
                effective,
            },
        });
        Outcome {
            response: Some(response),
            events: vec![event],
            close: false,
        }
    }
}

fn error(request_id: &str, code: ErrorCode, message: &str) -> ControlMessage {
    match ErrorMsg::new(request_id, code, message) {
        Ok(error) => ControlMessage::Error(error),
        Err(_) => ControlMessage::Error(
            ErrorMsg::new(request_id, ErrorCode::Internal, "error response failed")
                .expect("código y texto internos válidos"),
        ),
    }
}
