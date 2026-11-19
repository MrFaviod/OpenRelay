#![no_main]

//! Fuzz del decoder de datagrama de audio v1 (`audio-frame/v1`).
//!
//! Alimenta el decoder real de `openrelay-audio-core`: `type=1` PCM16LE de 508 B y
//! `type=2` Opus. No reimplementa el layout de la cabecera de 28 bytes.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = openrelay_audio_core::AudioPayload::decode(data);
});
