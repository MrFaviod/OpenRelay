//! Perfil PCM16LE mono 48 kHz de openrelay.
//!
//! Un frame fijo contiene 240 muestras (`5 ms` a `48 kHz`) en un solo canal:
//! 480 bytes little-endian. El perfil no negocia formato en v1.

use crate::frame::FrameError;

/// Frecuencia de muestreo fija del perfil v1.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
/// Canales del perfil v1 (mono).
pub const CHANNELS: u16 = 1;
/// Muestras por canal en un frame de 5 ms a 48 kHz.
pub const SAMPLE_COUNT: usize = 240;
/// Duración nominal del frame en microsegundos.
pub const DURATION_US: u16 = 5_000;
/// Bytes por muestra PCM16.
pub const BYTES_PER_SAMPLE: usize = 2;
/// Bytes de payload de un frame: 240 muestras x 1 canal x 2 bytes.
pub const PAYLOAD_LEN: usize = SAMPLE_COUNT * CHANNELS as usize * BYTES_PER_SAMPLE;

/// Muestras de un frame completo del perfil v1.
pub type Samples = [i16; SAMPLE_COUNT];

/// Payload silencioso del perfil v1.
pub const SILENCE: Samples = [0; SAMPLE_COUNT];

/// Serializa muestras a PCM16 little-endian sin asignar memoria.
pub fn to_le_bytes(samples: &Samples) -> [u8; PAYLOAD_LEN] {
    let mut out = [0u8; PAYLOAD_LEN];
    for (chunk, sample) in out.chunks_exact_mut(BYTES_PER_SAMPLE).zip(samples.iter()) {
        chunk.copy_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Decodifica exactamente 480 bytes PCM16 little-endian.
pub fn from_le_bytes(payload: &[u8]) -> Result<Samples, FrameError> {
    if payload.len() != PAYLOAD_LEN {
        return Err(FrameError::PayloadLength {
            expected: PAYLOAD_LEN,
            actual: payload.len(),
        });
    }
    let mut samples = SILENCE;
    for (sample, chunk) in samples
        .iter_mut()
        .zip(payload.chunks_exact(BYTES_PER_SAMPLE))
    {
        *sample = i16::from_le_bytes([chunk[0], chunk[1]]);
    }
    Ok(samples)
}
