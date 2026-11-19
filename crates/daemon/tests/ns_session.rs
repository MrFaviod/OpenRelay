//! Integración del NS opcional en el worker de sesión.
//!
//! Sin la feature `speexdsp` no hay etapa: pedir NS deja el camino en bypass y
//! las muestras salen idénticas. Con la feature, el NS pedido ejecuta una etapa
//! y declara su retardo; apagado sigue siendo bypass.

use arl_audio_core::{AudioFrame, AudioPayload, Flags, SAMPLE_COUNT};
use arl_daemon::session::SessionAudio;

/// Bloque determinista con variación suficiente para el NS.
fn block(seed: i16) -> [i16; SAMPLE_COUNT] {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample = (index as i16).wrapping_mul(97).wrapping_add(seed);
    }
    samples
}

/// Entrega un frame PCM16 y devuelve las muestras decodificadas.
fn run(audio: &mut SessionAudio, block: &[i16; SAMPLE_COUNT]) -> Vec<i16> {
    let frame = AudioFrame::new(5, 1, Flags::NONE, *block).expect("frame PCM16");
    let decoded = audio
        .handle(AudioPayload::Pcm16(frame))
        .expect("decodificación PCM16");
    assert_eq!(decoded.len(), 1, "un frame PCM16 produce un bloque");
    assert_eq!(decoded[0].epoch, 5);
    assert_eq!(decoded[0].seq, 1);
    decoded[0].samples.clone()
}

/// La sesión por defecto nunca ejecuta DSP ni altera las muestras.
#[test]
fn default_session_bypasses_without_dsp() {
    let input = block(11);
    let mut audio = SessionAudio::new(false);
    let output = run(&mut audio, &input);
    assert_eq!(output, input, "el bypass entrega las muestras intactas");
    let stats = audio.stats();
    assert_eq!(stats.ns_stages, 0, "ninguna etapa DSP");
    assert_eq!(stats.ns_frames_processed, 1, "un bloque contado");
    assert_eq!(stats.ns_frames_bypassed, 1, "un bloque en bypass");
    assert_eq!(stats.ns_added_latency_samples, 0, "sin retardo añadido");
}

/// Sin la feature, pedir NS no instala etapa: sigue siendo bypass exacto.
#[cfg(not(feature = "speexdsp"))]
#[test]
fn ns_request_without_feature_stays_bypass() {
    let input = block(23);
    let mut audio = SessionAudio::with_ns(false, true);
    let output = run(&mut audio, &input);
    assert_eq!(output, input, "sin feature no hay DSP que altere el bloque");
    let stats = audio.stats();
    assert_eq!(stats.ns_stages, 0, "ninguna etapa DSP");
    assert_eq!(stats.ns_frames_processed, 1, "un bloque contado");
    assert_eq!(stats.ns_frames_bypassed, 1, "un bloque en bypass");
    assert_eq!(stats.ns_added_latency_samples, 0, "sin retardo añadido");
}

/// Con la feature, el NS pedido ejecuta una etapa y declara un bloque de retardo.
#[cfg(feature = "speexdsp")]
#[test]
fn ns_enabled_runs_one_stage_and_declares_latency() {
    let input = block(37);
    let mut audio = SessionAudio::with_ns(false, true);
    let _output = run(&mut audio, &input);
    let stats = audio.stats();
    assert_eq!(stats.ns_stages, 1, "una etapa DSP ejecutada");
    assert_eq!(stats.ns_frames_processed, 1, "un bloque contado");
    assert_eq!(stats.ns_frames_bypassed, 0, "ningún bloque en bypass");
    assert_eq!(
        stats.ns_added_latency_samples, SAMPLE_COUNT as u64,
        "el NS declara una trama de retardo"
    );
}

/// Con la feature, apagar el NS conserva el bypass exacto.
#[cfg(feature = "speexdsp")]
#[test]
fn ns_disabled_with_feature_stays_bypass() {
    let input = block(53);
    let mut audio = SessionAudio::with_ns(false, false);
    let output = run(&mut audio, &input);
    assert_eq!(output, input, "el bypass no altera las muestras");
    let stats = audio.stats();
    assert_eq!(stats.ns_stages, 0, "ninguna etapa DSP");
    assert_eq!(stats.ns_frames_processed, 1, "un bloque contado");
    assert_eq!(stats.ns_frames_bypassed, 1, "un bloque en bypass");
    assert_eq!(stats.ns_added_latency_samples, 0, "sin retardo añadido");
}
