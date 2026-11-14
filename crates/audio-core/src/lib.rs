//! Core de audio de arl: frame v1, perfil PCM16LE mono 48 kHz, jitter buffer
//! acotado y control de drift con resampling.
//!
//! Este crate no hace red, no codifica, no toca dispositivos y no usa
//! `unsafe`. El frame es el contrato de `contracts/audio-frame-v1.schema.json`:
//! cabecera big-endian de 28 bytes y payload PCM16LE de 480 bytes. Jitter (T06)
//! y drift/resample (T07) consumen ese frame sin redefinir el contrato.
//!
//! Interfaz para consumidores (jitter/PLC en T06, captura en T14):
//!
//! ```
//! use arl_audio_core::{AudioFrame, Flags, DATAGRAM_LEN, SAMPLE_COUNT};
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
pub mod drift;
pub mod frame;
pub mod jitter;
pub mod metrics;
pub mod pcm16;
pub mod resample;

pub use frame::{
    AudioFrame, Flags, FrameError, FrameHeader, BYTES_PER_SAMPLE, CHANNELS, DATAGRAM_LEN,
    DURATION_US, HEADER_LEN, MAX_DATAGRAM_LEN, PAYLOAD_LEN, SAMPLE_COUNT, SAMPLE_RATE_HZ,
    TYPE_AUDIO, VERSION,
};
pub use pcm16::Samples;
