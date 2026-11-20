//! Core de audio de openrelay: frame v1, perfil PCM16LE mono 48 kHz, jitter buffer
//! acotado y control de drift con resampling.
//!
//! Este crate no hace red, no codifica, no toca dispositivos y no usa
//! `unsafe`. El frame es el contrato de `contracts/audio-frame-v1.schema.json`:
//! cabecera big-endian de 28 bytes y payload PCM16LE de 480 bytes. La
//! extensión compatible `type=2` (Opus 48 kHz mono, un paquete por datagrama)
//! sólo se usa si ambos extremos anuncian `audio.opus-48k-mono`; el perfil
//! PCM16 `type=1` queda byte a byte igual. Jitter y drift/resample
//! consumen ese frame sin redefinir el contrato.
//!
//! Interfaz para consumidores (jitter/PLC y captura):
//!
//! ```
//! use openrelay_audio_core::{AudioFrame, Flags, DATAGRAM_LEN, SAMPLE_COUNT};
//!
//! let mut samples = [0i16; SAMPLE_COUNT];
//! samples[0] = 1_000;
//! let frame = AudioFrame::new(1, 0, Flags::NONE, samples).expect("frame válido");
//! let wire: [u8; DATAGRAM_LEN] = frame.encode();
//! let decoded = AudioFrame::decode(&wire).expect("frame v1");
//! assert_eq!(decoded.seq(), 0);
//! assert_eq!(decoded.samples()[0], 1_000);
//! ```

#![forbid(unsafe_code)]

pub mod budget;
pub mod cadence;
pub mod drift;
pub mod frame;
pub mod jitter;
pub mod metrics;
pub mod pcm16;
pub mod resample;

pub use frame::{
    opus_duration_for, opus_sample_count_valid, AudioFormat, AudioFrame, AudioPayload, Flags,
    FrameError, FrameHeader, OpusFrame, BYTES_PER_SAMPLE, CHANNELS, DATAGRAM_LEN, DURATION_US,
    FEATURE_AUDIO_OPUS, FEATURE_AUDIO_PCM16, HEADER_LEN, MAX_DATAGRAM_LEN, OPUS_DURATIONS_US,
    OPUS_MAX_PAYLOAD_LEN, PAYLOAD_LEN, SAMPLE_COUNT, SAMPLE_RATE_HZ, TYPE_AUDIO, TYPE_AUDIO_OPUS,
    VERSION,
};
pub use pcm16::Samples;
