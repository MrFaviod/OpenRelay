//! Códec Opus en modo *restricted low delay* para openrelay.
//!
//! Este crate envuelve `libopus` (vía el crate seguro `opus`) y expone una API
//! mínima para el perfil de audio de openrelay: PCM16 mono a 48 kHz y tramas de
//! 2.5, 5 o 10 ms. El códec es **opcional**: el camino PCM16 fijo sigue siendo
//! el contrato por defecto y este crate no lo modifica.
//!
//! # Modo
//!
//! Todos los perfiles usan [`opus::Application::LowDelay`], que libopus mapea
//! a `OPUS_APPLICATION_RESTRICTED_LOWDELAY`. En ese modo el códec usa sólo
//! CELT, no SILK, y el *lookahead* es de 2.5 ms (120 muestras a 48 kHz). Como
//! consecuencia, la FEC en banda (que sólo existe en SILK) **no está
//! disponible**; la pérdida de paquetes se cubre con PLC al decodificar sin
//! paquete ([`OpusDecoder::decode`] con `None`).
//!
//! # Entrada
//!
//! libopus sólo acepta 8, 12, 16, 24 o 48 kHz; este crate fija 48 kHz. Una
//! fuente a 44.1 kHz debe remuestrearse a 48 kHz antes de llamar al codificador
//! (ver `docs/codec.md`). [`OpusEncoder::encode`] rechaza longitudes que no
//! correspondan exactamente al perfil.
//!
//! # Ejemplo
//!
//! ```
//! use openrelay_codec::{CodecProfile, OpusDecoder, OpusEncoder};
//!
//! let profile = CodecProfile::LowDelay5Ms;
//! let mut encoder = OpusEncoder::new(profile, 64_000).expect("encoder");
//! let mut decoder = OpusDecoder::new(profile).expect("decoder");
//!
//! let pcm = [0i16; 240];
//! let packet = encoder.encode(&pcm).expect("encode");
//! let decoded = decoder.decode(Some(&packet)).expect("decode");
//! assert_eq!(decoded.len(), 240);
//!
//! // Pérdida de paquete: PLC con la última duración conocida.
//! let concealed = decoder.decode(None).expect("PLC");
//! assert_eq!(concealed.len(), 240);
//! ```

#![forbid(unsafe_code)]

use core::fmt;

/// Frecuencia de muestreo del perfil de openrelay (Hz).
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// Canales del perfil de openrelay (mono).
pub const CHANNELS: u16 = 1;
/// Bitrate mínimo aceptado por los perfiles (bits/s).
pub const MIN_BITRATE_BPS: u32 = 64_000;
/// Bitrate máximo aceptado por los perfiles (bits/s).
pub const MAX_BITRATE_BPS: u32 = 96_000;
/// Tamaño máximo de un datagrama del transporte (bytes).
pub const MAX_DATAGRAM_LEN: usize = 1_200;
/// Cabecera del frame v1 de `openrelay-audio-core` (bytes).
pub const FRAME_HEADER_LEN: usize = 28;
/// Presupuesto máximo del paquete Opus dentro del datagrama (bytes).
pub const MAX_PACKET_LEN: usize = MAX_DATAGRAM_LEN - FRAME_HEADER_LEN;
/// *Lookahead* del modo restricted-lowdelay (microsegundos).
pub const LOOKAHEAD_US: u32 = 2_500;

/// Perfiles de trama soportados, todos en modo restricted-lowdelay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecProfile {
    /// Trama de 2.5 ms (120 muestras a 48 kHz).
    LowDelay2p5Ms,
    /// Trama de 5 ms (240 muestras a 48 kHz).
    LowDelay5Ms,
    /// Trama de 10 ms (480 muestras a 48 kHz).
    LowDelay10Ms,
}

impl CodecProfile {
    /// Todos los perfiles, en orden creciente de duración.
    pub const ALL: [Self; 3] = [Self::LowDelay2p5Ms, Self::LowDelay5Ms, Self::LowDelay10Ms];

    /// Muestras por canal en una trama del perfil.
    pub const fn samples_per_frame(self) -> usize {
        match self {
            Self::LowDelay2p5Ms => 120,
            Self::LowDelay5Ms => 240,
            Self::LowDelay10Ms => 480,
        }
    }

    /// Duración nominal de la trama en microsegundos.
    pub const fn frame_duration_us(self) -> u32 {
        match self {
            Self::LowDelay2p5Ms => 2_500,
            Self::LowDelay5Ms => 5_000,
            Self::LowDelay10Ms => 10_000,
        }
    }

    /// Deriva el perfil a partir del número de muestras por canal.
    pub const fn from_frame_samples(samples: usize) -> Result<Self, CodecError> {
        match samples {
            120 => Ok(Self::LowDelay2p5Ms),
            240 => Ok(Self::LowDelay5Ms),
            480 => Ok(Self::LowDelay10Ms),
            _ => Err(CodecError::InvalidFrameSamples { samples }),
        }
    }
}

/// Error tipado del crate.
#[derive(Debug)]
#[non_exhaustive]
pub enum CodecError {
    /// El número de muestras no corresponde a ningún perfil.
    InvalidFrameSamples {
        /// Muestras recibidas.
        samples: usize,
    },
    /// Bitrate fuera de `[MIN_BITRATE_BPS, MAX_BITRATE_BPS]`.
    InvalidBitrate {
        /// Bitrate recibido (bits/s).
        bitrate_bps: u32,
    },
    /// El buffer PCM no mide exactamente una trama del perfil.
    InvalidPcmLength {
        /// Muestras esperadas.
        expected: usize,
        /// Muestras recibidas.
        actual: usize,
    },
    /// El paquete supera el presupuesto del datagrama.
    PacketTooLarge {
        /// Longitud recibida.
        len: usize,
        /// Presupuesto máximo.
        max: usize,
    },
    /// libopus falló al codificar.
    Encode(opus::Error),
    /// libopus falló al decodificar.
    Decode(opus::Error),
    /// libopus devolvió un número de muestras distinto al del perfil.
    UnexpectedSampleCount {
        /// Muestras esperadas.
        expected: usize,
        /// Muestras devueltas.
        actual: usize,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFrameSamples { samples } => {
                write!(f, "invalid frame size: {samples} samples")
            }
            Self::InvalidBitrate { bitrate_bps } => write!(
                f,
                "invalid bitrate: {bitrate_bps} bps (range {}..={})",
                MIN_BITRATE_BPS, MAX_BITRATE_BPS
            ),
            Self::InvalidPcmLength { expected, actual } => {
                write!(f, "invalid PCM: expected {expected} samples, got {actual}")
            }
            Self::PacketTooLarge { len, max } => {
                write!(f, "packet of {len} bytes exceeds the maximum of {max}")
            }
            Self::Encode(error) => write!(f, "Opus encode error: {error}"),
            Self::Decode(error) => write!(f, "Opus decode error: {error}"),
            Self::UnexpectedSampleCount { expected, actual } => {
                write!(f, "expected {expected} decoded samples, got {actual}")
            }
        }
    }
}

impl std::error::Error for CodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Encode(error) | Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}

/// Codificador Opus restricted-lowdelay mono 48 kHz.
#[derive(Debug)]
pub struct OpusEncoder {
    inner: opus::Encoder,
    profile: CodecProfile,
    bitrate_bps: u32,
}

impl OpusEncoder {
    /// Crea un codificador para `profile` con el bitrate indicado.
    pub fn new(profile: CodecProfile, bitrate_bps: u32) -> Result<Self, CodecError> {
        if !(MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&bitrate_bps) {
            return Err(CodecError::InvalidBitrate { bitrate_bps });
        }
        let mut inner = opus::Encoder::new(
            SAMPLE_RATE_HZ,
            opus::Channels::Mono,
            opus::Application::LowDelay,
        )
        .map_err(CodecError::Encode)?;
        inner
            .set_bitrate(opus::Bitrate::Bits(bitrate_bps as i32))
            .map_err(CodecError::Encode)?;
        Ok(Self {
            inner,
            profile,
            bitrate_bps,
        })
    }

    /// Perfil configurado.
    pub const fn profile(&self) -> CodecProfile {
        self.profile
    }

    /// Bitrate nominal configurado (bits/s).
    pub const fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// *Lookahead* del códec en muestras a 48 kHz (2.5 ms en este modo).
    pub fn lookahead_samples(&mut self) -> Result<u32, CodecError> {
        self.inner
            .get_lookahead()
            .map(|value| value.max(0) as u32)
            .map_err(CodecError::Encode)
    }

    /// Codifica exactamente una trama del perfil.
    ///
    /// Devuelve un paquete de como máximo [`MAX_PACKET_LEN`] bytes.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>, CodecError> {
        let expected = self.profile.samples_per_frame();
        if pcm.len() != expected {
            return Err(CodecError::InvalidPcmLength {
                expected,
                actual: pcm.len(),
            });
        }
        let mut packet = vec![0u8; MAX_PACKET_LEN];
        let len = self
            .inner
            .encode(pcm, &mut packet)
            .map_err(CodecError::Encode)?;
        packet.truncate(len);
        Ok(packet)
    }

    /// Reinicia el estado del codificador.
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.inner.reset_state().map_err(CodecError::Encode)
    }
}

/// Decodificador Opus restricted-lowdelay mono 48 kHz.
#[derive(Debug)]
pub struct OpusDecoder {
    inner: opus::Decoder,
    profile: CodecProfile,
}

impl OpusDecoder {
    /// Crea un decodificador para `profile`.
    pub fn new(profile: CodecProfile) -> Result<Self, CodecError> {
        let inner =
            opus::Decoder::new(SAMPLE_RATE_HZ, opus::Channels::Mono).map_err(CodecError::Decode)?;
        Ok(Self { inner, profile })
    }

    /// Perfil configurado.
    pub const fn profile(&self) -> CodecProfile {
        self.profile
    }

    /// Decodifica un paquete, o aplica PLC si `packet` es `None`.
    ///
    /// Con `None` (o un paquete vacío) libopus oculta la pérdida usando la
    /// última duración de paquete conocida; la salida siempre mide exactamente
    /// una trama del perfil. La FEC en banda no se usa: no existe en el modo
    /// CELT/restricted-lowdelay.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<Vec<i16>, CodecError> {
        let packet = packet.unwrap_or(&[]);
        if packet.len() > MAX_PACKET_LEN {
            return Err(CodecError::PacketTooLarge {
                len: packet.len(),
                max: MAX_PACKET_LEN,
            });
        }
        let expected = self.profile.samples_per_frame();
        let mut pcm = vec![0i16; expected];
        let actual = self
            .inner
            .decode(packet, &mut pcm, false)
            .map_err(CodecError::Decode)?;
        if actual != expected {
            return Err(CodecError::UnexpectedSampleCount { expected, actual });
        }
        Ok(pcm)
    }

    /// Reinicia el estado del decodificador.
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.inner.reset_state().map_err(CodecError::Decode)
    }
}
