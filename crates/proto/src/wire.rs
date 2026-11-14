//! Wire de control `arl.control/v1`: framing, JSON estricto y mensajes.
//!
//! Un frame de control es `u32` big-endian con la longitud del payload
//! (1..=16384) seguido de exactamente ese número de bytes JSON UTF-8.
//!
//! El framing de audio **no** se define aquí: el contrato lo fija en
//! `docs/protocol.md` §8 (cabecera de 28 bytes, PCM16LE mono 48 kHz, 508 bytes
//! por datagrama, máximo aceptado 1200). Esta crate sólo transporta control.

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use crate::control::{
    validate_value, Capabilities, ConfigKey, ConfigKeyDeclaration, ConfigPair, ConfigScope,
    ConfigValue, ErrorCode, ErrorMsg, Feature, Limits, PeerRole, RemoteAction, SessionState,
    Snapshot, ValueType, CONFIG_SNAPSHOT_MAX_KEYS, MAX_CONFIG_KEYS_DECLARED, MAX_MESSAGE_CHARS,
    MAX_REQUEST_ID_CHARS,
};

/// Versión mayor del contrato que esta crate implementa.
pub const SCHEMA_VERSION: &str = "arl.control/v1";
/// Bytes del prefijo de longitud de control.
pub const CONTROL_LENGTH_BYTES: usize = 4;
/// Payload de control máximo, antes de asignar memoria proporcional.
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 16_384;
/// Profundidad máxima de JSON.
pub const MAX_JSON_DEPTH: usize = 8;
/// Claves máximas por objeto.
pub const MAX_OBJECT_PROPERTIES: usize = 64;
/// Elementos máximos por array.
pub const MAX_ARRAY_ITEMS: usize = 64;
/// Caracteres máximos por string JSON.
pub const MAX_STRING_CHARS: usize = 1024;
/// Revisión máxima exacta en JSON (2^53−1).
pub const MAX_REVISION: u64 = 9_007_199_254_740_991;

// ------------------------------------------------------------------- error --

/// Causa de rechazo de un frame de control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireErrorKind {
    /// Longitud declarada fuera de `1..=16384`, o frame con bytes de más.
    LengthOutOfRange,
    /// El frame declara más bytes de los recibidos.
    Truncated,
    /// Payload por encima del límite del contrato.
    TooLarge,
    /// JSON inválido, BOM, claves duplicadas o límites de forma excedidos.
    MalformedJson,
    /// `schema_version` o versión de protocolo desconocidas.
    UnsupportedVersion,
    /// Objeto válido en JSON pero que no cumple la forma de su tipo.
    InvalidShape,
}

/// Error de wire con el `request_id` recuperable, si el frame lo permitía.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    kind: WireErrorKind,
    request_id: Option<String>,
}

impl WireError {
    fn new(kind: WireErrorKind, request_id: Option<String>) -> Self {
        Self { kind, request_id }
    }

    fn shape(request_id: Option<String>) -> Self {
        Self::new(WireErrorKind::InvalidShape, request_id)
    }

    /// Rechazo de forma sin `request_id` recuperable.
    pub fn invalid(request_id: Option<String>) -> Self {
        Self::new(WireErrorKind::InvalidShape, request_id)
    }

    fn malformed() -> Self {
        Self::new(WireErrorKind::MalformedJson, None)
    }

    /// Categoría del rechazo.
    pub fn kind(&self) -> WireErrorKind {
        self.kind
    }

    /// `request_id` copiado del mensaje, si era legible.
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// Código del catálogo [`crate::control::ErrorCode`] que corresponde.
    pub fn code(&self) -> ErrorCode {
        match self.kind {
            WireErrorKind::TooLarge => ErrorCode::TooLarge,
            WireErrorKind::UnsupportedVersion => ErrorCode::VersionUnsupported,
            _ => ErrorCode::Invalid,
        }
    }

    /// Si el contrato exige cerrar la conexión ante este rechazo.
    pub fn closes_connection(&self) -> bool {
        match self.kind {
            WireErrorKind::InvalidShape => self.request_id.is_none(),
            _ => true,
        }
    }

    /// `error` listo para el wire, si se pudo recuperar `request_id`.
    pub fn to_error_message(&self) -> Option<ControlMessage> {
        let request_id = self
            .request_id
            .clone()
            .unwrap_or_else(|| "unknown".to_owned());
        let message = match self.kind {
            WireErrorKind::TooLarge => "control payload too large",
            WireErrorKind::UnsupportedVersion => "version unsupported",
            WireErrorKind::Truncated | WireErrorKind::LengthOutOfRange => "invalid control frame",
            WireErrorKind::MalformedJson => "malformed control json",
            WireErrorKind::InvalidShape => "invalid control message",
        };
        ErrorMsg::new(&request_id, self.code(), message)
            .ok()
            .map(ControlMessage::Error)
    }
}

// -------------------------------------------------------------- json value --

/// Número JSON, sin perder la distinción entero/con coma flotante.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JsonNumber {
    /// Entero con signo.
    Integer(i64),
    /// Entero sin signo.
    Unsigned(u64),
    /// Número con parte fraccionaria o exponente.
    Float(f64),
}

impl JsonNumber {
    /// Valor no negativo y exacto como `u64`, si lo hay.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            JsonNumber::Integer(value) if value >= 0 => Some(value as u64),
            JsonNumber::Unsigned(value) => Some(value),
            JsonNumber::Float(value)
                if value.is_finite()
                    && value.fract() == 0.0
                    && value >= 0.0
                    && value <= MAX_REVISION as f64 =>
            {
                Some(value as u64)
            }
            _ => None,
        }
    }

    /// Valor como `f64`.
    pub fn as_f64(&self) -> f64 {
        match *self {
            JsonNumber::Integer(value) => value as f64,
            JsonNumber::Unsigned(value) => value as f64,
            JsonNumber::Float(value) => value,
        }
    }
}

/// Valor JSON validado y acotado, con claves duplicadas ya rechazadas.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    /// `null`.
    Null,
    /// Booleano.
    Bool(bool),
    /// Número.
    Number(JsonNumber),
    /// String UTF-8.
    String(String),
    /// Array.
    Array(Vec<JsonValue>),
    /// Objeto como lista ordenada de pares, sin duplicados.
    Object(Vec<(String, JsonValue)>),
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("un valor JSON acotado")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(JsonNumber::Integer(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(JsonNumber::Unsigned(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsonValue::Number(JsonNumber::Float(value)))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Null)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.chars().count() > MAX_STRING_CHARS {
            return Err(E::custom("string too long"));
        }
        Ok(JsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.chars().count() > MAX_STRING_CHARS {
            return Err(E::custom("string too long"));
        }
        Ok(JsonValue::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::new();
        while let Some(item) = sequence.next_element::<JsonValue>()? {
            if items.len() == MAX_ARRAY_ITEMS {
                return Err(serde::de::Error::custom("array too large"));
            }
            items.push(item);
        }
        Ok(JsonValue::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries: Vec<(String, JsonValue)> = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            if entries.len() == MAX_OBJECT_PROPERTIES {
                return Err(serde::de::Error::custom("object too large"));
            }
            if key.chars().count() > MAX_STRING_CHARS {
                return Err(serde::de::Error::custom("key too long"));
            }
            if entries.iter().any(|(existing, _)| existing == &key) {
                return Err(serde::de::Error::custom("duplicate key"));
            }
            let value = map.next_value::<JsonValue>()?;
            entries.push((key, value));
        }
        Ok(JsonValue::Object(entries))
    }
}

/// Parsea el payload JSON de control con todos los límites del contrato.
///
/// Rechaza BOM, claves duplicadas, números no estándar, bytes de más,
/// profundidad > 8, objetos > 64 claves, arrays > 64 elementos y strings
/// > 1024 caracteres. El límite de tamaño ya se aplicó en [`decode_frame`].
///
/// Frontera de bajo nivel: valida la forma JSON, no la semántica del mensaje;
/// eso lo hace [`decode_message`].
pub fn parse_json(payload: &[u8]) -> Result<JsonValue, WireError> {
    if payload.is_empty() || payload.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(WireError::malformed());
    }
    let value: JsonValue = serde_json::from_slice(payload).map_err(|_| WireError::malformed())?;
    if json_depth(&value) > MAX_JSON_DEPTH {
        return Err(WireError::malformed());
    }
    Ok(value)
}

fn json_depth(value: &JsonValue) -> usize {
    match value {
        JsonValue::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        JsonValue::Object(entries) => {
            1 + entries
                .iter()
                .map(|(_, v)| json_depth(v))
                .max()
                .unwrap_or(0)
        }
        _ => 0,
    }
}

/// Serializa un [`JsonValue`] a JSON UTF-8 compacto.
///
/// Frontera de bajo nivel: no valida el schema ni el mensaje, y un `Float` no
/// finito se escribe como `null`. Para emitir un frame decodificable usa
/// [`encode_message`].
pub fn encode_json(value: &JsonValue) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

fn write_json(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Number(number) => match *number {
            JsonNumber::Integer(value) => out.push_str(&value.to_string()),
            JsonNumber::Unsigned(value) => out.push_str(&value.to_string()),
            JsonNumber::Float(value) => write_float(value, out),
        },
        JsonValue::String(text) => write_string(text, out),
        JsonValue::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        JsonValue::Object(entries) => {
            out.push('{');
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

fn write_float(value: f64, out: &mut String) {
    if value.is_finite() {
        out.push_str(&format_float(value));
    } else {
        // JSON no puede representar NaN/±Infinity. El serializador de bajo
        // nivel escribe `null` en vez de inventar `0.0` (que sí decodificaría
        // como un número real); `encode_message` rechaza estos valores mucho
        // antes, así que este camino sólo es alcanzable con un `JsonValue`
        // construido a mano.
        out.push_str("null");
    }
}

fn format_float(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_owned();
    }
    let normalized = if value == 0.0 { 0.0 } else { value };
    if normalized.fract() == 0.0 && normalized.abs() < 1e15 {
        format!("{normalized:.1}")
    } else {
        format!("{normalized}")
    }
}

// ----------------------------------------------------------------- framing --

/// Valida el prefijo `u32` BE y devuelve el payload exacto del frame.
///
/// Frontera de bajo nivel: no parsea JSON ni valida el mensaje; eso es
/// [`decode_message`].
pub fn decode_frame(frame: &[u8]) -> Result<&[u8], WireError> {
    if frame.len() < CONTROL_LENGTH_BYTES {
        return Err(WireError::new(WireErrorKind::LengthOutOfRange, None));
    }
    let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if declared == 0 {
        return Err(WireError::new(WireErrorKind::LengthOutOfRange, None));
    }
    if declared > MAX_CONTROL_PAYLOAD_BYTES {
        return Err(WireError::new(WireErrorKind::TooLarge, None));
    }
    let total = CONTROL_LENGTH_BYTES + declared;
    if frame.len() < total {
        return Err(WireError::new(WireErrorKind::Truncated, None));
    }
    if frame.len() > total {
        return Err(WireError::new(WireErrorKind::LengthOutOfRange, None));
    }
    Ok(&frame[CONTROL_LENGTH_BYTES..])
}

/// Antepone el prefijo `u32` BE a un payload ya serializado.
///
/// Frontera de bajo nivel: sólo aplica los límites de tamaño, no valida el
/// contenido; un payload inválido produce un frame que el peer rechazará.
pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, WireError> {
    if payload.is_empty() {
        return Err(WireError::new(WireErrorKind::LengthOutOfRange, None));
    }
    if payload.len() > MAX_CONTROL_PAYLOAD_BYTES {
        return Err(WireError::new(WireErrorKind::TooLarge, None));
    }
    let mut frame = Vec::with_capacity(CONTROL_LENGTH_BYTES + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

// ----------------------------------------------------------------- mensajes --

/// `hello` del cliente.
#[derive(Debug, Clone, PartialEq)]
pub struct HelloMsg {
    /// Identificador de la operación.
    pub request_id: String,
    /// Rol declarado.
    pub role: PeerRole,
    /// Versión de la aplicación.
    pub version: String,
    /// Versión de protocolo; debe ser `arl.control/v1`.
    pub protocol_version: String,
}

/// `capabilities` en cualquier dirección.
#[derive(Debug, Clone, PartialEq)]
pub struct CapabilitiesMsg {
    /// Identificador de la operación.
    pub request_id: String,
    /// Revisión de configuración actual.
    pub revision: u64,
    /// Capacidades declaradas.
    pub capabilities: Capabilities,
}

/// `config.get`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigGetMsg {
    /// Identificador de la operación.
    pub request_id: String,
    /// Clave concreta, o `None` para el snapshot completo.
    pub key: Option<ConfigKey>,
}

/// `config.set`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSetMsg {
    /// Identificador de la operación.
    pub request_id: String,
    /// Revisión que el cliente cree vigente.
    pub expected_revision: u64,
    /// Clave a escribir.
    pub key: ConfigKey,
    /// Valor solicitado.
    pub value: ConfigValue,
}

/// Efectivo de un `ack`: par para `config.set`, snapshot para `config.get`.
#[derive(Debug, Clone, PartialEq)]
pub enum AckEffective {
    /// Par `{key, value}`.
    Pair(ConfigPair),
    /// Snapshot `clave → valor`.
    Snapshot(Snapshot),
}

/// `ack` del daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct AckMsg {
    /// `request_id` eco.
    pub request_id: String,
    /// Revisión resultante.
    pub revision: u64,
    /// Si el estado cambió y la revisión avanzó.
    pub applied: bool,
    /// Lo solicitado; `null` en respuestas a `config.get`.
    pub requested: Option<ConfigPair>,
    /// Lo aplicado; nunca es `null`.
    pub effective: AckEffective,
}

/// `event` del daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct EventMsg {
    /// Revisión del evento.
    pub revision: u64,
    /// Cuerpo cerrado del evento.
    pub event: EventBody,
}

/// Eventos v1, con `data` cerrada y plana.
#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    /// Cambio de configuración aplicado.
    ConfigChanged {
        /// Clave.
        key: ConfigKey,
        /// Valor solicitado.
        requested: ConfigValue,
        /// Valor efectivo.
        effective: ConfigValue,
    },
    /// Cambio de estado de sesión.
    SessionState {
        /// Estado.
        state: SessionState,
        /// Detalle corto opcional.
        detail: Option<String>,
    },
}

/// Mensaje de control `arl.control/v1`.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlMessage {
    /// `hello` del cliente.
    Hello(HelloMsg),
    /// `capabilities` en cualquier dirección.
    Capabilities(CapabilitiesMsg),
    /// `config.get`.
    ConfigGet(ConfigGetMsg),
    /// `config.set`.
    ConfigSet(ConfigSetMsg),
    /// `ack` del daemon.
    Ack(AckMsg),
    /// `error` del daemon.
    Error(ErrorMsg),
    /// `event` del daemon.
    Event(EventMsg),
    /// `ping`.
    Ping {
        /// Identificador de la operación.
        request_id: String,
    },
    /// `pong`.
    Pong {
        /// Identificador de la operación.
        request_id: String,
    },
}

impl ControlMessage {
    /// `request_id` si el tipo lo lleva.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            ControlMessage::Hello(message) => Some(&message.request_id),
            ControlMessage::Capabilities(message) => Some(&message.request_id),
            ControlMessage::ConfigGet(message) => Some(&message.request_id),
            ControlMessage::ConfigSet(message) => Some(&message.request_id),
            ControlMessage::Ack(message) => Some(&message.request_id),
            ControlMessage::Error(message) => Some(&message.request_id),
            ControlMessage::Event(_) => None,
            ControlMessage::Ping { request_id } | ControlMessage::Pong { request_id } => {
                Some(request_id)
            }
        }
    }
}

/// Decodifica un frame completo a un mensaje tipado.
///
/// Es la puerta semántica del decoder y la referencia que [`encode_message`]
/// usa para garantizar que todo lo que emite se puede volver a leer.
pub fn decode_message(frame: &[u8]) -> Result<ControlMessage, WireError> {
    let payload = decode_frame(frame)?;
    let value = parse_json(payload)?;
    decode_value(value)
}

/// Codifica un mensaje tipado, validando sus campos antes de emitir.
///
/// Es la puerta semántica del encoder: toda salida `Ok` es un frame que
/// [`decode_message`] devuelve como el mismo mensaje. Para garantizarlo sin
/// duplicar reglas, el frame se pasa por el propio decodificador compartido y
/// cualquier divergencia se rechaza como [`WireErrorKind::InvalidShape`] en vez
/// de normalizarse. Los constructores públicos de los structs permiten crear
/// combinaciones que el contrato no admite (revisiones fuera de rango, valores
/// que no cumplen su clave, `error` incoherente, `hello` con otra versión de
/// protocolo); todas se rechazan aquí.
pub fn encode_message(message: &ControlMessage) -> Result<Vec<u8>, WireError> {
    let value = message_to_json(message)?;
    let payload = encode_json(&value);
    let frame = encode_frame(payload.as_bytes())?;
    let echo = message.request_id().map(str::to_owned);
    let decoded = decode_message(&frame).map_err(|_| WireError::shape(echo.clone()))?;
    if decoded != *message {
        return Err(WireError::shape(echo));
    }
    Ok(frame)
}

fn is_valid_request_id(request_id: &str) -> bool {
    let count = request_id.chars().count();
    (1..=MAX_REQUEST_ID_CHARS).contains(&count)
        && request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Regla única de `version` en `hello`, compartida por encoder y decoder.
fn is_valid_version(version: &str) -> bool {
    let count = version.chars().count();
    (1..=64).contains(&count)
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

fn require_request_id(value: String) -> Result<String, WireError> {
    if is_valid_request_id(&value) {
        Ok(value)
    } else {
        Err(WireError::shape(None))
    }
}

fn message_to_json(message: &ControlMessage) -> Result<JsonValue, WireError> {
    match message {
        ControlMessage::Hello(hello) => {
            require_request_id(hello.request_id.clone())?;
            if !is_valid_version(&hello.version) {
                return Err(shape_error(Some(&hello.request_id)));
            }
            if hello.protocol_version != SCHEMA_VERSION {
                return Err(WireError::new(
                    WireErrorKind::UnsupportedVersion,
                    Some(hello.request_id.clone()),
                ));
            }
            envelope(
                "hello",
                Some(&hello.request_id),
                None,
                None,
                JsonValue::Object(vec![
                    ("role".into(), JsonValue::String(hello.role.as_str().into())),
                    ("version".into(), JsonValue::String(hello.version.clone())),
                    (
                        "protocol_version".into(),
                        JsonValue::String(hello.protocol_version.clone()),
                    ),
                ]),
            )
        }
        ControlMessage::Capabilities(message) => {
            require_request_id(message.request_id.clone())?;
            let payload = capabilities_to_json(&message.capabilities, Some(&message.request_id))?;
            envelope(
                "capabilities",
                Some(&message.request_id),
                Some(message.revision),
                None,
                payload,
            )
        }
        ControlMessage::ConfigGet(message) => {
            require_request_id(message.request_id.clone())?;
            let key = match &message.key {
                Some(key) => JsonValue::String(key.as_str().to_owned()),
                None => JsonValue::Null,
            };
            envelope(
                "config.get",
                Some(&message.request_id),
                None,
                None,
                JsonValue::Object(vec![("key".into(), key)]),
            )
        }
        ControlMessage::ConfigSet(message) => {
            require_request_id(message.request_id.clone())?;
            validate_key_value(&message.key, &message.value, Some(&message.request_id))?;
            envelope(
                "config.set",
                Some(&message.request_id),
                None,
                Some(message.expected_revision),
                JsonValue::Object(vec![
                    (
                        "key".into(),
                        JsonValue::String(message.key.as_str().to_owned()),
                    ),
                    (
                        "value".into(),
                        config_value_to_json(&message.value, Some(&message.request_id))?,
                    ),
                ]),
            )
        }
        ControlMessage::Ack(ack) => {
            require_request_id(ack.request_id.clone())?;
            let request_id = Some(ack.request_id.as_str());
            let requested = match &ack.requested {
                Some(pair) => pair_to_json(pair, request_id)?,
                None => JsonValue::Null,
            };
            let effective = match &ack.effective {
                AckEffective::Pair(pair) => pair_to_json(pair, request_id)?,
                AckEffective::Snapshot(snapshot) => snapshot_to_json(snapshot, request_id)?,
            };
            envelope(
                "ack",
                request_id,
                Some(ack.revision),
                None,
                JsonValue::Object(vec![
                    ("applied".into(), JsonValue::Bool(ack.applied)),
                    ("requested".into(), requested),
                    ("effective".into(), effective),
                ]),
            )
        }
        ControlMessage::Error(error) => {
            require_request_id(error.request_id.clone())?;
            let request_id = Some(error.request_id.as_str());
            if error.message.is_empty() || error.message.chars().count() > MAX_MESSAGE_CHARS {
                return Err(shape_error(request_id));
            }
            if error.retryable != error.code.retryable() {
                return Err(shape_error(request_id));
            }
            let current_revision = match (error.code, error.current_revision) {
                (ErrorCode::Conflict, Some(revision)) => {
                    Some(revision_to_json(revision, request_id)?)
                }
                (ErrorCode::Conflict, None) | (_, Some(_)) => return Err(shape_error(request_id)),
                (_, None) => None,
            };
            let mut payload = vec![
                ("code".into(), JsonValue::String(error.code.as_str().into())),
                ("message".into(), JsonValue::String(error.message.clone())),
                ("retryable".into(), JsonValue::Bool(error.retryable)),
            ];
            if let Some(revision) = current_revision {
                payload.push(("current_revision".into(), revision));
            }
            envelope("error", request_id, None, None, JsonValue::Object(payload))
        }
        ControlMessage::Event(event) => {
            let (name, data) = match &event.event {
                EventBody::ConfigChanged {
                    key,
                    requested,
                    effective,
                } => {
                    validate_key_value(key, requested, None)?;
                    validate_key_value(key, effective, None)?;
                    (
                        "config.changed",
                        JsonValue::Object(vec![
                            ("key".into(), JsonValue::String(key.as_str().to_owned())),
                            ("requested".into(), config_value_to_json(requested, None)?),
                            ("effective".into(), config_value_to_json(effective, None)?),
                        ]),
                    )
                }
                EventBody::SessionState { state, detail } => {
                    let mut fields =
                        vec![("state".into(), JsonValue::String(state.as_str().into()))];
                    if let Some(detail) = detail {
                        if detail.chars().count() > MAX_MESSAGE_CHARS {
                            return Err(WireError::shape(None));
                        }
                        fields.push(("detail".into(), JsonValue::String(detail.clone())));
                    }
                    ("session.state", JsonValue::Object(fields))
                }
            };
            envelope(
                "event",
                None,
                Some(event.revision),
                None,
                JsonValue::Object(vec![
                    ("name".into(), JsonValue::String(name.into())),
                    ("data".into(), data),
                ]),
            )
        }
        ControlMessage::Ping { request_id } => {
            require_request_id(request_id.clone())?;
            envelope(
                "ping",
                Some(request_id),
                None,
                None,
                JsonValue::Object(vec![]),
            )
        }
        ControlMessage::Pong { request_id } => {
            require_request_id(request_id.clone())?;
            envelope(
                "pong",
                Some(request_id),
                None,
                None,
                JsonValue::Object(vec![]),
            )
        }
    }
}

/// Construye el sobre común.
///
/// `revision` y `expected_revision` se propagan con `Result`: una revisión por
/// encima de [`MAX_REVISION`] no se omite en silencio (eso produciría un frame
/// que el decodificador rechaza o, peor, un mensaje sin su revisión), se
/// rechaza con el `request_id` disponible.
fn envelope(
    type_name: &str,
    request_id: Option<&str>,
    revision: Option<u64>,
    expected_revision: Option<u64>,
    payload: JsonValue,
) -> Result<JsonValue, WireError> {
    let mut entries: Vec<(String, JsonValue)> = vec![
        (
            "schema_version".into(),
            JsonValue::String(SCHEMA_VERSION.into()),
        ),
        ("type".into(), JsonValue::String(type_name.into())),
    ];
    if let Some(request_id) = request_id {
        entries.push(("request_id".into(), JsonValue::String(request_id.into())));
    }
    if let Some(expected_revision) = expected_revision {
        entries.push((
            "expected_revision".into(),
            revision_to_json(expected_revision, request_id)?,
        ));
    }
    if let Some(revision) = revision {
        entries.push(("revision".into(), revision_to_json(revision, request_id)?));
    }
    entries.push(("payload".into(), payload));
    Ok(JsonValue::Object(entries))
}

fn revision_to_json(revision: u64, request_id: Option<&str>) -> Result<JsonValue, WireError> {
    if revision > MAX_REVISION {
        return Err(shape_error(request_id));
    }
    Ok(JsonValue::Number(JsonNumber::Unsigned(revision)))
}

fn config_value_to_json(
    value: &ConfigValue,
    request_id: Option<&str>,
) -> Result<JsonValue, WireError> {
    let json = match value {
        ConfigValue::Bool(value) => JsonValue::Bool(*value),
        ConfigValue::Integer(value) => JsonValue::Number(JsonNumber::Integer(*value)),
        ConfigValue::Number(value) => {
            // `null`/`NaN`/`Infinity` no son JSON y `format_float` no debe
            // inventar un número; el schema acota además a ±1e6.
            if !value.is_finite() || value.abs() > 1_000_000.0 {
                return Err(shape_error(request_id));
            }
            JsonValue::Number(JsonNumber::Float(*value))
        }
        ConfigValue::String(value) => {
            if value.chars().count() > crate::control::MAX_CONFIG_STRING_CHARS {
                return Err(shape_error(request_id));
            }
            JsonValue::String(value.clone())
        }
    };
    Ok(json)
}

/// Valida un valor contra la clave cuando la clave pertenece al catálogo.
///
/// Mismo criterio que el decodificador: una clave desconocida no se valida
/// aquí (el daemon la rechaza en la máquina de control); una conocida debe
/// cumplir tipo, rango y paso.
fn validate_key_value(
    key: &ConfigKey,
    value: &ConfigValue,
    request_id: Option<&str>,
) -> Result<(), WireError> {
    if let Some(spec) = crate::control::lookup_key(key.as_str()) {
        validate_value(spec, value).map_err(|_| shape_error(request_id))?;
    }
    Ok(())
}

fn pair_to_json(pair: &ConfigPair, request_id: Option<&str>) -> Result<JsonValue, WireError> {
    validate_key_value(&pair.key, &pair.value, request_id)?;
    Ok(JsonValue::Object(vec![
        (
            "key".into(),
            JsonValue::String(pair.key.as_str().to_owned()),
        ),
        (
            "value".into(),
            config_value_to_json(&pair.value, request_id)?,
        ),
    ]))
}

fn snapshot_to_json(snapshot: &Snapshot, request_id: Option<&str>) -> Result<JsonValue, WireError> {
    if snapshot.len() > CONFIG_SNAPSHOT_MAX_KEYS {
        return Err(shape_error(request_id));
    }
    // `effective` es `pair | snapshot`: un snapshot con exactamente las claves
    // `key` y `value` (ambas legales como clave de configuración) es
    // indistinguible de un par y el decodificador elegiría `Pair`.
    if snapshot.len() == 2
        && snapshot.iter().any(|(key, _)| key.as_str() == "key")
        && snapshot.iter().any(|(key, _)| key.as_str() == "value")
    {
        return Err(shape_error(request_id));
    }
    let mut entries = Vec::with_capacity(snapshot.len());
    for (key, value) in snapshot.iter() {
        validate_key_value(key, value, request_id)?;
        entries.push((
            key.as_str().to_owned(),
            config_value_to_json(value, request_id)?,
        ));
    }
    Ok(JsonValue::Object(entries))
}

fn capabilities_to_json(
    capabilities: &Capabilities,
    request_id: Option<&str>,
) -> Result<JsonValue, WireError> {
    if !(8..=64).contains(&capabilities.device_id.chars().count())
        || !capabilities
            .device_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(shape_error(request_id));
    }
    if capabilities.features.len() > 8 || has_duplicates(&capabilities.features) {
        return Err(shape_error(request_id));
    }
    if capabilities.remote_actions.len() > 2 || has_duplicates(&capabilities.remote_actions) {
        return Err(shape_error(request_id));
    }
    // Los límites son constantes del contrato renegociadas por `capabilities`:
    // el decodificador sólo acepta los valores exactos.
    if capabilities.limits != Limits::default() {
        return Err(shape_error(request_id));
    }
    if capabilities.config_keys.len() > MAX_CONFIG_KEYS_DECLARED {
        return Err(shape_error(request_id));
    }
    for declaration in &capabilities.config_keys {
        for bound in [declaration.minimum, declaration.maximum]
            .into_iter()
            .flatten()
        {
            if !bound.is_finite() {
                return Err(shape_error(request_id));
            }
        }
    }
    Ok(JsonValue::Object(vec![
        (
            "schema_version".into(),
            JsonValue::String("arl.capabilities/v1".into()),
        ),
        (
            "device_id".into(),
            JsonValue::String(capabilities.device_id.clone()),
        ),
        (
            "role".into(),
            JsonValue::String(capabilities.role.as_str().into()),
        ),
        (
            "features".into(),
            JsonValue::Array(
                capabilities
                    .features
                    .iter()
                    .map(|feature| JsonValue::String(feature.as_str().into()))
                    .collect(),
            ),
        ),
        (
            "remote_actions".into(),
            JsonValue::Array(
                capabilities
                    .remote_actions
                    .iter()
                    .map(|action| JsonValue::String(action.as_str().into()))
                    .collect(),
            ),
        ),
        (
            "limits".into(),
            JsonValue::Object(vec![
                (
                    "max_control_payload_bytes".into(),
                    JsonValue::Number(JsonNumber::Unsigned(
                        capabilities.limits.max_control_payload_bytes,
                    )),
                ),
                (
                    "max_audio_datagram_bytes".into(),
                    JsonValue::Number(JsonNumber::Unsigned(
                        capabilities.limits.max_audio_datagram_bytes,
                    )),
                ),
                (
                    "max_reorder_ms".into(),
                    JsonValue::Number(JsonNumber::Unsigned(capabilities.limits.max_reorder_ms)),
                ),
                (
                    "max_config_keys".into(),
                    JsonValue::Number(JsonNumber::Unsigned(capabilities.limits.max_config_keys)),
                ),
                (
                    "max_request_id_len".into(),
                    JsonValue::Number(JsonNumber::Unsigned(capabilities.limits.max_request_id_len)),
                ),
            ]),
        ),
        (
            "config_keys".into(),
            JsonValue::Array(
                capabilities
                    .config_keys
                    .iter()
                    .map(declaration_to_json)
                    .collect(),
            ),
        ),
    ]))
}

fn has_duplicates<T: PartialEq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[..index].contains(item))
}

fn declaration_to_json(declaration: &ConfigKeyDeclaration) -> JsonValue {
    let mut fields = vec![
        (
            "key".into(),
            JsonValue::String(declaration.key.as_str().to_owned()),
        ),
        (
            "scope".into(),
            JsonValue::String(declaration.scope.as_str().into()),
        ),
        (
            "type".into(),
            JsonValue::String(declaration.value_type.as_str().into()),
        ),
    ];
    if let Some(minimum) = declaration.minimum {
        fields.push((
            "minimum".into(),
            JsonValue::Number(JsonNumber::Float(minimum)),
        ));
    }
    if let Some(maximum) = declaration.maximum {
        fields.push((
            "maximum".into(),
            JsonValue::Number(JsonNumber::Float(maximum)),
        ));
    }
    JsonValue::Object(fields)
}

// ---------------------------------------------------------------- decoding --

struct Fields {
    entries: Vec<(String, JsonValue)>,
}

impl Fields {
    fn parse(value: JsonValue) -> Result<Self, WireError> {
        match value {
            JsonValue::Object(entries) => Ok(Self { entries }),
            _ => Err(WireError::shape(None)),
        }
    }

    fn peek(&self, name: &str) -> Option<&JsonValue> {
        self.entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    fn peek_request_id(&self) -> Option<String> {
        match self.peek("request_id") {
            Some(JsonValue::String(value)) if is_valid_request_id(value) => Some(value.clone()),
            _ => None,
        }
    }

    fn take(&mut self, name: &str) -> Option<JsonValue> {
        let position = self.entries.iter().position(|(key, _)| key == name)?;
        Some(self.entries.remove(position).1)
    }

    fn require(&mut self, name: &str, request_id: Option<&str>) -> Result<JsonValue, WireError> {
        self.take(name)
            .ok_or_else(|| WireError::shape(request_id.map(str::to_owned)))
    }

    fn require_string(
        &mut self,
        name: &str,
        request_id: Option<&str>,
    ) -> Result<String, WireError> {
        match self.require(name, request_id)? {
            JsonValue::String(value) => Ok(value),
            _ => Err(WireError::shape(request_id.map(str::to_owned))),
        }
    }

    fn require_request_id(&mut self) -> Result<String, WireError> {
        let value = self.require_string("request_id", None)?;
        require_request_id(value)
    }

    fn require_revision(&mut self, name: &str) -> Result<u64, WireError> {
        match self.require(name, None)? {
            JsonValue::Number(number) => number
                .as_u64()
                .filter(|value| *value <= MAX_REVISION)
                .ok_or_else(|| WireError::shape(None)),
            _ => Err(WireError::shape(None)),
        }
    }

    fn require_object(&mut self, name: &str) -> Result<Fields, WireError> {
        Fields::parse(self.require(name, None)?)
    }

    fn reject(&mut self, name: &str) -> Result<(), WireError> {
        if self.take(name).is_some() {
            return Err(WireError::shape(None));
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), WireError> {
        if self.entries.is_empty() {
            Ok(())
        } else {
            Err(WireError::shape(None))
        }
    }
}

fn shape_error(request_id: Option<&str>) -> WireError {
    WireError::shape(request_id.map(str::to_owned))
}

fn decode_config_key(value: JsonValue, request_id: Option<&str>) -> Result<ConfigKey, WireError> {
    match value {
        JsonValue::String(name) => ConfigKey::new(&name).map_err(|_| shape_error(request_id)),
        _ => Err(shape_error(request_id)),
    }
}

fn decode_revision_value(value: &JsonValue) -> Result<u64, WireError> {
    match value {
        JsonValue::Number(number) => number
            .as_u64()
            .filter(|revision| *revision <= MAX_REVISION)
            .ok_or_else(|| WireError::shape(None)),
        _ => Err(WireError::shape(None)),
    }
}

fn decode_config_value(
    value: JsonValue,
    request_id: Option<&str>,
) -> Result<ConfigValue, WireError> {
    match value {
        JsonValue::Bool(value) => Ok(ConfigValue::Bool(value)),
        JsonValue::Number(JsonNumber::Integer(value)) => Ok(ConfigValue::Integer(value)),
        JsonValue::Number(JsonNumber::Unsigned(value)) => i64::try_from(value)
            .map(ConfigValue::Integer)
            .map_err(|_| shape_error(request_id)),
        JsonValue::Number(JsonNumber::Float(value)) => {
            if value.is_finite() && value.abs() <= 1_000_000.0 {
                Ok(ConfigValue::Number(value))
            } else {
                Err(shape_error(request_id))
            }
        }
        JsonValue::String(text) => {
            if text.chars().count() <= crate::control::MAX_CONFIG_STRING_CHARS {
                Ok(ConfigValue::String(text))
            } else {
                Err(shape_error(request_id))
            }
        }
        _ => Err(shape_error(request_id)),
    }
}

fn decode_pair(value: JsonValue, request_id: Option<&str>) -> Result<ConfigPair, WireError> {
    let mut fields = Fields::parse(value)?;
    let key = decode_config_key(fields.require("key", request_id)?, request_id)?;
    let value = decode_config_value(fields.require("value", request_id)?, request_id)?;
    fields.finish()?;
    validate_key_value(&key, &value, request_id)?;
    Ok(ConfigPair { key, value })
}

fn decode_snapshot(value: JsonValue, request_id: Option<&str>) -> Result<Snapshot, WireError> {
    let object = match value {
        JsonValue::Object(entries) => entries,
        _ => return Err(shape_error(request_id)),
    };
    if object.len() > crate::control::CONFIG_SNAPSHOT_MAX_KEYS {
        return Err(shape_error(request_id));
    }
    let mut snapshot = Snapshot::new();
    for (name, value) in object {
        let key = ConfigKey::new(&name).map_err(|_| shape_error(request_id))?;
        let value = decode_config_value(value, request_id)?;
        validate_key_value(&key, &value, request_id)?;
        snapshot.insert(key, value);
    }
    Ok(snapshot)
}

fn decode_value(value: JsonValue) -> Result<ControlMessage, WireError> {
    let mut fields = Fields::parse(value)?;
    let request_id_hint = fields.peek_request_id();
    let version = fields.require_string("schema_version", request_id_hint.as_deref())?;
    if version != SCHEMA_VERSION {
        return Err(WireError::new(
            WireErrorKind::UnsupportedVersion,
            request_id_hint,
        ));
    }
    let type_name = fields.require_string("type", request_id_hint.as_deref())?;
    let message = match type_name.as_str() {
        "hello" => decode_hello(&mut fields)?,
        "capabilities" => decode_capabilities(&mut fields)?,
        "config.get" => decode_config_get(&mut fields)?,
        "config.set" => decode_config_set(&mut fields)?,
        "ack" => decode_ack(&mut fields)?,
        "error" => decode_error(&mut fields)?,
        "event" => decode_event(&mut fields)?,
        "ping" => {
            let request_id = fields.require_request_id()?;
            fields.reject("revision")?;
            fields.reject("expected_revision")?;
            let mut payload = fields.require_object("payload")?;
            payload.finish()?;
            fields.finish()?;
            ControlMessage::Ping { request_id }
        }
        "pong" => {
            let request_id = fields.require_request_id()?;
            fields.reject("revision")?;
            fields.reject("expected_revision")?;
            let mut payload = fields.require_object("payload")?;
            payload.finish()?;
            fields.finish()?;
            ControlMessage::Pong { request_id }
        }
        _ => return Err(WireError::shape(request_id_hint)),
    };
    Ok(message)
}

fn decode_hello(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let request_id = fields.require_request_id()?;
    fields.reject("revision")?;
    fields.reject("expected_revision")?;
    let payload = fields.require_object("payload")?;
    fields.finish()?;
    let mut payload = payload;
    let role = decode_role(payload.require("role", Some(&request_id))?)?;
    let version = payload.require_string("version", Some(&request_id))?;
    if !is_valid_version(&version) {
        return Err(shape_error(Some(&request_id)));
    }
    let protocol_version = payload.require_string("protocol_version", Some(&request_id))?;
    payload.finish()?;
    if protocol_version != SCHEMA_VERSION {
        return Err(WireError::new(
            WireErrorKind::UnsupportedVersion,
            Some(request_id),
        ));
    }
    Ok(ControlMessage::Hello(HelloMsg {
        request_id,
        role,
        version,
        protocol_version,
    }))
}

fn decode_role(value: JsonValue) -> Result<PeerRole, WireError> {
    match value {
        JsonValue::String(name) => match name.as_str() {
            "android" => Ok(PeerRole::Android),
            "linux-cli" => Ok(PeerRole::LinuxCli),
            "linux-daemon" => Ok(PeerRole::LinuxDaemon),
            _ => Err(WireError::shape(None)),
        },
        _ => Err(WireError::shape(None)),
    }
}

fn decode_feature(value: JsonValue) -> Result<Feature, WireError> {
    match value {
        JsonValue::String(name) => match name.as_str() {
            "control.v1" => Ok(Feature::ControlV1),
            "audio.pcm16-48k-mono" => Ok(Feature::AudioPcm16_48kMono),
            "enrollment.v1" => Ok(Feature::EnrollmentV1),
            "session.v1" => Ok(Feature::SessionV1),
            _ => Err(WireError::shape(None)),
        },
        _ => Err(WireError::shape(None)),
    }
}

fn decode_remote_action(value: JsonValue) -> Result<RemoteAction, WireError> {
    match value {
        JsonValue::String(name) => match name.as_str() {
            "config.read" => Ok(RemoteAction::ConfigRead),
            "config.write" => Ok(RemoteAction::ConfigWrite),
            _ => Err(WireError::shape(None)),
        },
        _ => Err(WireError::shape(None)),
    }
}

fn decode_capabilities(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let request_id = fields.require_request_id()?;
    let revision = fields.require_revision("revision")?;
    let payload = fields.require_object("payload")?;
    fields.finish()?;
    let capabilities = capabilities_from_fields(payload, Some(&request_id))?;
    Ok(ControlMessage::Capabilities(CapabilitiesMsg {
        request_id,
        revision,
        capabilities,
    }))
}

fn capabilities_from_fields(
    mut payload: Fields,
    request_id: Option<&str>,
) -> Result<Capabilities, WireError> {
    let schema_version = payload.require_string("schema_version", request_id)?;
    if schema_version != "arl.capabilities/v1" {
        return Err(WireError::new(
            WireErrorKind::UnsupportedVersion,
            request_id.map(str::to_owned),
        ));
    }
    let device_id = payload.require_string("device_id", request_id)?;
    if !(8..=64).contains(&device_id.chars().count())
        || !device_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(shape_error(request_id));
    }
    let role = decode_role(payload.require("role", request_id)?)?;
    let features_value = payload.require("features", request_id)?;
    let features = decode_array(features_value, 8, decode_feature)?;
    let actions_value = payload.require("remote_actions", request_id)?;
    let remote_actions = decode_array(actions_value, 2, decode_remote_action)?;
    let mut limits = payload.require_object("limits")?;
    let max_control_payload_bytes =
        require_const_unsigned(&mut limits, "max_control_payload_bytes", 16_384)?;
    let max_audio_datagram_bytes =
        require_const_unsigned(&mut limits, "max_audio_datagram_bytes", 1_200)?;
    let max_reorder_ms = require_const_unsigned(&mut limits, "max_reorder_ms", 20)?;
    let max_config_keys = require_const_unsigned(&mut limits, "max_config_keys", 32)?;
    let max_request_id_len = require_const_unsigned(&mut limits, "max_request_id_len", 64)?;
    limits.finish()?;
    let declarations_value = payload.require("config_keys", request_id)?;
    let declarations = match declarations_value {
        JsonValue::Array(items) => {
            if items.len() > 32 {
                return Err(shape_error(request_id));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(declaration_from_fields(Fields::parse(item)?, request_id)?);
            }
            out
        }
        _ => return Err(shape_error(request_id)),
    };
    payload.finish()?;
    Ok(Capabilities {
        device_id,
        role,
        features,
        remote_actions,
        limits: crate::control::Limits {
            max_control_payload_bytes,
            max_audio_datagram_bytes,
            max_reorder_ms,
            max_config_keys,
            max_request_id_len,
        },
        config_keys: declarations,
    })
}

fn decode_array<T>(
    value: JsonValue,
    max_items: usize,
    decode: fn(JsonValue) -> Result<T, WireError>,
) -> Result<Vec<T>, WireError> {
    match value {
        JsonValue::Array(items) => {
            if items.len() > max_items {
                return Err(WireError::shape(None));
            }
            items.into_iter().map(decode).collect()
        }
        _ => Err(WireError::shape(None)),
    }
}

fn require_const_unsigned(
    fields: &mut Fields,
    name: &str,
    expected: u64,
) -> Result<u64, WireError> {
    match fields.require(name, None)? {
        JsonValue::Number(number) => match number.as_u64() {
            Some(value) if value == expected => Ok(value),
            _ => Err(WireError::shape(None)),
        },
        _ => Err(WireError::shape(None)),
    }
}

fn declaration_from_fields(
    mut fields: Fields,
    request_id: Option<&str>,
) -> Result<ConfigKeyDeclaration, WireError> {
    let key = decode_config_key(fields.require("key", request_id)?, request_id)?;
    let scope = match fields.require("scope", request_id)? {
        JsonValue::String(name) => match name.as_str() {
            "shared" => ConfigScope::Shared,
            "local" => ConfigScope::Local,
            "readonly" => ConfigScope::Readonly,
            _ => return Err(shape_error(request_id)),
        },
        _ => return Err(shape_error(request_id)),
    };
    let value_type = match fields.require("type", request_id)? {
        JsonValue::String(name) => match name.as_str() {
            "boolean" => ValueType::Boolean,
            "integer" => ValueType::Integer,
            "number" => ValueType::Number,
            "string" => ValueType::String,
            _ => return Err(shape_error(request_id)),
        },
        _ => return Err(shape_error(request_id)),
    };
    let minimum = match fields.take("minimum") {
        Some(JsonValue::Number(number)) => Some(number.as_f64()),
        Some(_) => return Err(shape_error(request_id)),
        None => None,
    };
    let maximum = match fields.take("maximum") {
        Some(JsonValue::Number(number)) => Some(number.as_f64()),
        Some(_) => return Err(shape_error(request_id)),
        None => None,
    };
    fields.finish()?;
    Ok(ConfigKeyDeclaration {
        key,
        scope,
        value_type,
        minimum,
        maximum,
    })
}

fn decode_config_get(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let request_id = fields.require_request_id()?;
    fields.reject("revision")?;
    fields.reject("expected_revision")?;
    let mut payload = fields.require_object("payload")?;
    fields.finish()?;
    let key = match payload.require("key", Some(&request_id))? {
        JsonValue::Null => None,
        value => Some(decode_config_key(value, Some(&request_id))?),
    };
    payload.finish()?;
    Ok(ControlMessage::ConfigGet(ConfigGetMsg { request_id, key }))
}

fn decode_config_set(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let request_id = fields.require_request_id()?;
    let expected_revision = fields.require_revision("expected_revision")?;
    fields.reject("revision")?;
    let mut payload = fields.require_object("payload")?;
    fields.finish()?;
    let key = decode_config_key(
        payload.require("key", Some(&request_id))?,
        Some(&request_id),
    )?;
    let value = decode_config_value(
        payload.require("value", Some(&request_id))?,
        Some(&request_id),
    )?;
    payload.finish()?;
    validate_key_value(&key, &value, Some(&request_id))?;
    Ok(ControlMessage::ConfigSet(ConfigSetMsg {
        request_id,
        expected_revision,
        key,
        value,
    }))
}

fn decode_ack(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let request_id = fields.require_request_id()?;
    let revision = fields.require_revision("revision")?;
    fields.reject("expected_revision")?;
    let mut payload = fields.require_object("payload")?;
    fields.finish()?;
    let applied = match payload.require("applied", Some(&request_id))? {
        JsonValue::Bool(value) => value,
        _ => return Err(shape_error(Some(&request_id))),
    };
    let requested = match payload.require("requested", Some(&request_id))? {
        JsonValue::Null => None,
        value => Some(decode_pair(value, Some(&request_id))?),
    };
    let effective_value = payload.require("effective", Some(&request_id))?;
    payload.finish()?;
    let effective = if is_pair_object(&effective_value) {
        AckEffective::Pair(decode_pair(effective_value, Some(&request_id))?)
    } else {
        AckEffective::Snapshot(decode_snapshot(effective_value, Some(&request_id))?)
    };
    Ok(ControlMessage::Ack(AckMsg {
        request_id,
        revision,
        applied,
        requested,
        effective,
    }))
}

fn is_pair_object(value: &JsonValue) -> bool {
    match value {
        JsonValue::Object(entries) => {
            entries.len() == 2
                && entries.iter().any(|(key, _)| key == "key")
                && entries.iter().any(|(key, _)| key == "value")
        }
        _ => false,
    }
}

fn decode_error(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let request_id = fields.require_request_id()?;
    fields.reject("revision")?;
    fields.reject("expected_revision")?;
    let mut payload = fields.require_object("payload")?;
    fields.finish()?;
    let code = match payload.require("code", Some(&request_id))? {
        JsonValue::String(name) => {
            ErrorCode::parse(&name).ok_or_else(|| shape_error(Some(&request_id)))?
        }
        _ => return Err(shape_error(Some(&request_id))),
    };
    let message = payload.require_string("message", Some(&request_id))?;
    if message.is_empty() || message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(shape_error(Some(&request_id)));
    }
    let retryable = match payload.require("retryable", Some(&request_id))? {
        JsonValue::Bool(value) => value,
        _ => return Err(shape_error(Some(&request_id))),
    };
    if retryable != code.retryable() {
        return Err(shape_error(Some(&request_id)));
    }
    let current_revision = match payload.take("current_revision") {
        Some(value) => Some(decode_revision_value(&value)?),
        None => None,
    };
    payload.finish()?;
    match (code, current_revision) {
        (ErrorCode::Conflict, Some(revision)) => Ok(ControlMessage::Error(ErrorMsg {
            request_id,
            code,
            message,
            retryable,
            current_revision: Some(revision),
        })),
        (ErrorCode::Conflict, None) => Err(shape_error(Some(&request_id))),
        (_, None) => Ok(ControlMessage::Error(ErrorMsg {
            request_id,
            code,
            message,
            retryable,
            current_revision: None,
        })),
        (_, Some(_)) => Err(shape_error(Some(&request_id))),
    }
}

fn decode_event(fields: &mut Fields) -> Result<ControlMessage, WireError> {
    let revision = fields.require_revision("revision")?;
    fields.reject("request_id")?;
    fields.reject("expected_revision")?;
    let mut payload = fields.require_object("payload")?;
    fields.finish()?;
    let name = payload.require_string("name", None)?;
    let mut data = payload.require_object("data")?;
    payload.finish()?;
    let event = match name.as_str() {
        "config.changed" => {
            let key = decode_config_key(data.require("key", None)?, None)?;
            let requested = decode_config_value(data.require("requested", None)?, None)?;
            let effective = decode_config_value(data.require("effective", None)?, None)?;
            data.finish()?;
            validate_key_value(&key, &requested, None)?;
            validate_key_value(&key, &effective, None)?;
            EventBody::ConfigChanged {
                key,
                requested,
                effective,
            }
        }
        "session.state" => {
            let state = match data.require("state", None)? {
                JsonValue::String(name) => match name.as_str() {
                    "active" => SessionState::Active,
                    "closed" => SessionState::Closed,
                    _ => return Err(WireError::shape(None)),
                },
                _ => return Err(WireError::shape(None)),
            };
            let detail = match data.take("detail") {
                Some(JsonValue::String(detail)) => {
                    if detail.chars().count() > MAX_MESSAGE_CHARS {
                        return Err(WireError::shape(None));
                    }
                    Some(detail)
                }
                Some(_) => return Err(WireError::shape(None)),
                None => None,
            };
            data.finish()?;
            EventBody::SessionState { state, detail }
        }
        _ => return Err(WireError::shape(None)),
    };
    Ok(ControlMessage::Event(EventMsg { revision, event }))
}

/// Comprueba que el mensaje codificado no supera el frame máximo.
pub fn encoded_len(message: &ControlMessage) -> Result<usize, WireError> {
    encode_message(message).map(|frame| frame.len())
}
