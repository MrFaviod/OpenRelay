//! Apertura opt-in de un dispositivo ALSA `hw:`.
//!
//! ALSA no es una ruta automática: el usuario tiene que pedir un dispositivo
//! `hw:` concreto en su configuración. Si no lo pide, [`open_from_config`]
//! devuelve `None` sin tocar el hardware. Cualquier otro nombre (`default`,
//! `plughw:`, `sysdefault`) se rechaza antes de abrir nada.
//!
//! El módulo no ejecuta procesos: no llama a `modprobe`, no carga módulos del
//! kernel y no necesita privilegios. La apertura la hace `libasound` del
//! sistema, enlazada dinámicamente; este crate no usa FFI propio.

use std::fmt;

use alsa::pcm::PCM;
use alsa::Direction;

/// Prefijo obligatorio de un dispositivo ALSA explícito.
pub const HW_PREFIX: &str = "hw:";

/// Configuración del usuario para el fallback ALSA.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AlsaConfig {
    /// Dispositivo `hw:` pedido explícitamente; `None` deja el fallback apagado.
    pub hw_device: Option<String>,
}

impl AlsaConfig {
    /// Configuración sin fallback ALSA, que es el valor por defecto.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Configuración que pide explícitamente un dispositivo `hw:`.
    pub fn hw(device: impl Into<String>) -> Self {
        Self {
            hw_device: Some(device.into()),
        }
    }
}

/// Error del fallback ALSA.
#[derive(Debug)]
pub enum AlsaError {
    /// El dispositivo pedido no empieza por `hw:`; no se adivina otro.
    NotHwDevice(String),
    /// `libasound` no pudo abrir el dispositivo de captura.
    Open {
        /// Dispositivo pedido, tal como lo escribió el usuario.
        device: String,
        /// Mensaje de `libasound`.
        message: String,
    },
}

impl fmt::Display for AlsaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlsaError::NotHwDevice(device) => {
                write!(
                    f,
                    "el fallback ALSA exige un dispositivo hw: explícito, no {device}"
                )
            }
            AlsaError::Open { device, message } => {
                write!(f, "no se pudo abrir {device} para captura: {message}")
            }
        }
    }
}

impl std::error::Error for AlsaError {}

/// PCM de captura abierto en un dispositivo `hw:`.
pub struct AlsaHwSource {
    pcm: PCM,
    device: String,
}

impl AlsaHwSource {
    /// Abre `device` en modo captura.
    ///
    /// Sólo acepta el prefijo `hw:`: no hay selección implícita ni reintento
    /// con otro dispositivo.
    pub fn open(device: &str) -> Result<Self, AlsaError> {
        if !device.starts_with(HW_PREFIX) {
            return Err(AlsaError::NotHwDevice(device.to_string()));
        }
        let pcm = PCM::new(device, Direction::Capture, false).map_err(|error| AlsaError::Open {
            device: device.to_string(),
            message: error.to_string(),
        })?;
        Ok(Self {
            pcm,
            device: device.to_string(),
        })
    }

    /// Dispositivo abierto, tal como lo pidió el usuario.
    pub fn device(&self) -> &str {
        &self.device
    }

    /// PCM de `libasound`, para que el consumidor fije el formato del contrato.
    pub fn pcm(&self) -> &PCM {
        &self.pcm
    }
}

impl fmt::Debug for AlsaHwSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlsaHwSource")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

/// Abre el fallback sólo si la configuración lo pide explícitamente.
///
/// Sin `hw_device` devuelve `Ok(None)` y no hay ninguna llamada a `libasound`.
pub fn open_from_config(config: &AlsaConfig) -> Result<Option<AlsaHwSource>, AlsaError> {
    match config.hw_device.as_deref() {
        None => Ok(None),
        Some(device) => AlsaHwSource::open(device).map(Some),
    }
}
