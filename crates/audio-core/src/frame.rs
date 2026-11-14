//! Frame de audio v1: cabecera big-endian de 28 bytes y payload PCM16LE de 480.
//!
//! El layout y las constantes son los de `docs/protocol.md` §8 y
//! `contracts/audio-frame-v1.schema.json`. `decode` rechaza cualquier datagrama
//! que no sea exactamente un frame de 508 bytes; no fragmenta ni reordena.

use core::fmt;
use core::ops::{BitOr, BitOrAssign};

use crate::pcm16::{self, Samples};

pub use crate::pcm16::{
    BYTES_PER_SAMPLE, CHANNELS, DURATION_US, PAYLOAD_LEN, SAMPLE_COUNT, SAMPLE_RATE_HZ,
};

/// Bytes de cabecera de un frame v1.
pub const HEADER_LEN: usize = 28;
/// Bytes totales de un frame v1 (cabecera + payload).
pub const DATAGRAM_LEN: usize = HEADER_LEN + PAYLOAD_LEN;
/// Tamaño máximo de datagrama aceptado por el transporte v1.
pub const MAX_DATAGRAM_LEN: usize = 1_200;
/// Tipo de datagrama `audio` del protocolo v1.
pub const TYPE_AUDIO: u8 = 1;
/// Versión del frame v1.
pub const VERSION: u8 = 1;

/// Error tipado de construcción o parseo de un frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameError {
    /// El datagrama no llega a la cabecera mínima.
    TooShort { len: usize },
    /// El datagrama supera el máximo aceptado por el transporte.
    Oversized { len: usize },
    /// El datagrama no mide exactamente un frame v1.
    LengthMismatch { expected: usize, actual: usize },
    /// El byte `type` no es `1`.
    InvalidType(u8),
    /// El byte `version` no es `1`.
    InvalidVersion(u8),
    /// El byte `flags` usa bits reservados.
    InvalidFlags { found: u8 },
    /// El byte `header_len` no es `28`.
    InvalidHeaderLen(u8),
    /// La época es `0`.
    InvalidEpoch,
    /// `sample_count` no es `240`.
    InvalidSampleCount(u16),
    /// `duration_us` no es `5000`.
    InvalidDuration(u16),
    /// `sample_rate_hz` no es `48000`.
    InvalidSampleRate(u32),
    /// `channels` no es `1`.
    InvalidChannels(u16),
    /// `reserved` no es `0`.
    InvalidReserved(u16),
    /// `payload_len` no es `480`.
    InvalidPayloadLen(u32),
    /// Un payload PCM16 no mide exactamente 480 bytes.
    PayloadLength { expected: usize, actual: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => {
                write!(
                    f,
                    "datagrama de {len} B: no llega a la cabecera de {HEADER_LEN} B"
                )
            }
            Self::Oversized { len } => {
                write!(
                    f,
                    "datagrama de {len} B: excede el máximo de {MAX_DATAGRAM_LEN} B"
                )
            }
            Self::LengthMismatch { expected, actual } => {
                write!(
                    f,
                    "datagrama de {actual} B: se esperaba exactamente {expected} B"
                )
            }
            Self::InvalidType(found) => write!(f, "type {found}: se esperaba {TYPE_AUDIO}"),
            Self::InvalidVersion(found) => write!(f, "version {found}: se esperaba {VERSION}"),
            Self::InvalidFlags { found } => {
                write!(f, "flags 0x{found:02x}: bits 2-7 reservados en cero")
            }
            Self::InvalidHeaderLen(found) => {
                write!(f, "header_len {found}: se esperaba {HEADER_LEN}")
            }
            Self::InvalidEpoch => write!(f, "epoch 0: inválido"),
            Self::InvalidSampleCount(found) => {
                write!(f, "sample_count {found}: se esperaba {SAMPLE_COUNT}")
            }
            Self::InvalidDuration(found) => {
                write!(f, "duration_us {found}: se esperaba {DURATION_US}")
            }
            Self::InvalidSampleRate(found) => {
                write!(f, "sample_rate_hz {found}: se esperaba {SAMPLE_RATE_HZ}")
            }
            Self::InvalidChannels(found) => {
                write!(f, "channels {found}: se esperaba {CHANNELS}")
            }
            Self::InvalidReserved(found) => {
                write!(f, "reserved {found}: se esperaba 0")
            }
            Self::InvalidPayloadLen(found) => {
                write!(f, "payload_len {found}: se esperaba {PAYLOAD_LEN}")
            }
            Self::PayloadLength { expected, actual } => {
                write!(f, "payload de {actual} B: se esperaban {expected} B PCM16")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Flags de un frame v1: sólo los bits 0 (`silence`) y 1 (`discontinuity`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(u8);

impl Flags {
    /// Sin flags.
    pub const NONE: Self = Self(0);
    /// Payload silencioso; sigue presente y con tamaño fijo.
    pub const SILENCE: Self = Self(0x01);
    /// Primer frame tras un hueco o cambio de época.
    pub const DISCONTINUITY: Self = Self(0x02);
    /// Máscara de bits definidos en v1.
    pub const MASK: u8 = 0x03;

    /// Valida y construye flags; los bits 2-7 deben ser cero.
    pub fn new(bits: u8) -> Result<Self, FrameError> {
        if bits & !Self::MASK != 0 {
            return Err(FrameError::InvalidFlags { found: bits });
        }
        Ok(Self(bits))
    }

    /// Bits crudos del byte `flags`.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// `true` si el bit `silence` está puesto.
    pub const fn is_silence(self) -> bool {
        self.0 & Self::SILENCE.0 != 0
    }

    /// `true` si el bit `discontinuity` está puesto.
    pub const fn is_discontinuity(self) -> bool {
        self.0 & Self::DISCONTINUITY.0 != 0
    }
}

impl BitOr for Flags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Campos variables de la cabecera v1; el resto son constantes del contrato.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    epoch: u32,
    seq: u32,
    flags: Flags,
}

impl FrameHeader {
    /// Construye una cabecera válida. `epoch` debe ser distinto de cero.
    pub fn new(epoch: u32, seq: u32, flags: Flags) -> Result<Self, FrameError> {
        if epoch == 0 {
            return Err(FrameError::InvalidEpoch);
        }
        Ok(Self { epoch, seq, flags })
    }

    /// Generación del flujo; nunca `0`.
    pub const fn epoch(self) -> u32 {
        self.epoch
    }

    /// Secuencia por frame dentro de la época; el wrap es válido.
    pub const fn seq(self) -> u32 {
        self.seq
    }

    /// Flags validados del frame.
    pub const fn flags(self) -> Flags {
        self.flags
    }
}

/// Un frame de audio v1 completo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFrame {
    header: FrameHeader,
    samples: Samples,
}

impl AudioFrame {
    /// Construye un frame con muestras del perfil fijo.
    pub fn new(epoch: u32, seq: u32, flags: Flags, samples: Samples) -> Result<Self, FrameError> {
        Ok(Self {
            header: FrameHeader::new(epoch, seq, flags)?,
            samples,
        })
    }

    /// Cabecera del frame (copia; no expone campos mutables).
    pub const fn header(&self) -> FrameHeader {
        self.header
    }

    /// Época del frame.
    pub const fn epoch(&self) -> u32 {
        self.header.epoch
    }

    /// Secuencia del frame.
    pub const fn seq(&self) -> u32 {
        self.header.seq
    }

    /// Flags del frame.
    pub const fn flags(&self) -> Flags {
        self.header.flags
    }

    /// Muestras PCM16 del frame.
    pub fn samples(&self) -> &Samples {
        &self.samples
    }

    /// Muestras mutables para procesamiento local (ganancia, PLC, etc.).
    pub fn samples_mut(&mut self) -> &mut Samples {
        &mut self.samples
    }

    /// Serializa el frame completo en un buffer fijo, sin asignar memoria.
    pub fn encode_into(&self, out: &mut [u8; DATAGRAM_LEN]) {
        out[0] = TYPE_AUDIO;
        out[1] = VERSION;
        out[2] = self.header.flags.bits();
        out[3] = HEADER_LEN as u8;
        out[4..8].copy_from_slice(&self.header.epoch.to_be_bytes());
        out[8..12].copy_from_slice(&self.header.seq.to_be_bytes());
        out[12..14].copy_from_slice(&(SAMPLE_COUNT as u16).to_be_bytes());
        out[14..16].copy_from_slice(&DURATION_US.to_be_bytes());
        out[16..20].copy_from_slice(&SAMPLE_RATE_HZ.to_be_bytes());
        out[20..22].copy_from_slice(&CHANNELS.to_be_bytes());
        out[22..24].copy_from_slice(&0u16.to_be_bytes());
        out[24..28].copy_from_slice(&(PAYLOAD_LEN as u32).to_be_bytes());
        out[HEADER_LEN..].copy_from_slice(&pcm16::to_le_bytes(&self.samples));
    }

    /// Serializa el frame completo en un array fijo, sin asignar memoria.
    pub fn encode(&self) -> [u8; DATAGRAM_LEN] {
        let mut out = [0u8; DATAGRAM_LEN];
        self.encode_into(&mut out);
        out
    }

    /// Decodifica exactamente un frame v1 de 508 bytes.
    pub fn decode(datagram: &[u8]) -> Result<Self, FrameError> {
        if datagram.len() < HEADER_LEN {
            return Err(FrameError::TooShort {
                len: datagram.len(),
            });
        }
        if datagram.len() > MAX_DATAGRAM_LEN {
            return Err(FrameError::Oversized {
                len: datagram.len(),
            });
        }
        if datagram.len() != DATAGRAM_LEN {
            return Err(FrameError::LengthMismatch {
                expected: DATAGRAM_LEN,
                actual: datagram.len(),
            });
        }

        if datagram[0] != TYPE_AUDIO {
            return Err(FrameError::InvalidType(datagram[0]));
        }
        if datagram[1] != VERSION {
            return Err(FrameError::InvalidVersion(datagram[1]));
        }
        let flags = Flags::new(datagram[2])?;
        if datagram[3] != HEADER_LEN as u8 {
            return Err(FrameError::InvalidHeaderLen(datagram[3]));
        }
        let epoch = read_u32(datagram, 4);
        if epoch == 0 {
            return Err(FrameError::InvalidEpoch);
        }
        let seq = read_u32(datagram, 8);
        let sample_count = read_u16(datagram, 12);
        if sample_count != SAMPLE_COUNT as u16 {
            return Err(FrameError::InvalidSampleCount(sample_count));
        }
        let duration = read_u16(datagram, 14);
        if duration != DURATION_US {
            return Err(FrameError::InvalidDuration(duration));
        }
        let sample_rate = read_u32(datagram, 16);
        if sample_rate != SAMPLE_RATE_HZ {
            return Err(FrameError::InvalidSampleRate(sample_rate));
        }
        let channels = read_u16(datagram, 20);
        if channels != CHANNELS {
            return Err(FrameError::InvalidChannels(channels));
        }
        let reserved = read_u16(datagram, 22);
        if reserved != 0 {
            return Err(FrameError::InvalidReserved(reserved));
        }
        let payload_len = read_u32(datagram, 24);
        if payload_len != PAYLOAD_LEN as u32 {
            return Err(FrameError::InvalidPayloadLen(payload_len));
        }

        let samples = pcm16::from_le_bytes(&datagram[HEADER_LEN..])?;
        Ok(Self {
            header: FrameHeader { epoch, seq, flags },
            samples,
        })
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}
