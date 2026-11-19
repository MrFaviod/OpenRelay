//! Pruebas del bypass real y del supresor SpeexDSP de `openrelay-dsp`.
//!
//! Sin feature se demuestra que el bypass no ejecuta etapas, no altera la
//! muestra y declara retardo cero. Con `speexdsp` se demuestra que la etapa
//! corre, declara retardo y atenúa ruido blanco en las tres tramas de openrelay.

use openrelay_dsp::{NoiseSuppressor, NsChain, NsCounters, NsError};

/// Ruido blanco determinista en el rango completo de i16.
fn noise_block(len: usize, seed: u32) -> Vec<i16> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            ((state >> 16) as i32 - 32_768) as i16
        })
        .collect()
}

/// RMS de un bloque PCM16.
#[cfg(feature = "speexdsp")]
fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum();
    (sum / samples.len() as f64).sqrt()
}

#[test]
fn bypass_runs_zero_stages_and_is_sample_identical() {
    // `bypass` es `const`: construirla no reserva nada.
    const BYPASS: NsChain = NsChain::bypass();
    let mut chain = BYPASS;
    assert!(chain.is_bypass());
    assert_eq!(chain.frame_samples(), None);
    assert_eq!(chain.added_latency_samples(), 0);

    for len in [120, 240, 480] {
        let original = noise_block(len, 7 + len as u32);
        let mut block = original.clone();
        for _ in 0..5 {
            assert!(!chain.process(&mut block));
        }
        assert_eq!(block, original, "el bypass alteró un bloque de {len}");
    }

    let counters = chain.counters();
    assert_eq!(counters.stages_run, 0);
    assert_eq!(counters.frames_processed, 15);
    assert_eq!(counters.frames_bypassed, 15);
    assert_eq!(counters.frames_rejected, 0);
}

/// Etapa falsa que sólo suma uno: prueba que el contrato admite otra
/// implementación y que el retardo se toma de la etapa.
struct CountingStage;

impl NoiseSuppressor for CountingStage {
    fn process(&mut self, samples: &mut [i16]) {
        for sample in samples {
            *sample = sample.wrapping_add(1);
        }
    }

    fn added_latency_samples(&self) -> usize {
        120
    }
}

#[test]
fn stage_is_interchangeable_and_reports_its_own_latency() {
    let mut chain = NsChain::with_stage(Box::new(CountingStage), 240).expect("cadena");
    assert!(!chain.is_bypass());
    assert_eq!(chain.added_latency_samples(), 120);

    let mut block = [0i16; 240];
    assert!(chain.process(&mut block));
    assert!(block.iter().all(|sample| *sample == 1));
    assert_eq!(
        chain.counters(),
        NsCounters {
            stages_run: 1,
            frames_processed: 1,
            frames_bypassed: 0,
            frames_rejected: 0,
        }
    );
}

#[test]
fn zero_frame_stage_is_rejected() {
    assert_eq!(
        NsChain::with_stage(Box::new(CountingStage), 0).err(),
        Some(NsError::InvalidParams)
    );
}

#[cfg(feature = "speexdsp")]
#[test]
fn speexdsp_runs_one_stage_and_declares_latency() {
    let mut chain = NsChain::speexdsp(48_000, 240).expect("cadena SpeexDSP");
    assert!(!chain.is_bypass());
    assert_eq!(chain.frame_samples(), Some(240));
    assert_eq!(chain.added_latency_samples(), 240);

    let mut block = noise_block(240, 11);
    assert!(chain.process(&mut block));
    assert_eq!(
        chain.counters(),
        NsCounters {
            stages_run: 1,
            frames_processed: 1,
            frames_bypassed: 0,
            frames_rejected: 0,
        }
    );
}

#[cfg(feature = "speexdsp")]
#[test]
fn speexdsp_rejects_invalid_parameters() {
    assert_eq!(
        NsChain::speexdsp(48_000, 0).err(),
        Some(NsError::InvalidParams)
    );
    assert_eq!(
        NsChain::speexdsp(0, 240).err(),
        Some(NsError::InvalidParams)
    );
}

#[cfg(feature = "speexdsp")]
#[test]
fn speexdsp_attenuates_white_noise() {
    let mut chain = NsChain::speexdsp(48_000, 240).expect("cadena SpeexDSP");
    let mut input_rms = 0.0;
    let mut output_rms = 0.0;

    // El NS necesita adaptarse: se mide el último bloque tras 60 de calentamiento.
    for block_index in 0..60 {
        let original = noise_block(240, 1_000 + block_index);
        let mut block = original.clone();
        assert!(chain.process(&mut block));
        if block_index == 59 {
            input_rms = rms(&original);
            output_rms = rms(&block);
        }
    }

    let ratio = output_rms / input_rms;
    assert!(
        ratio < 0.5,
        "el NS no atenuó el ruido: entrada {input_rms:.1}, salida {output_rms:.1}, ratio {ratio:.3}"
    );
}

#[cfg(feature = "speexdsp")]
#[test]
fn speexdsp_handles_every_openrelay_profile_block() {
    for frame_samples in [120, 240, 480] {
        let mut chain = NsChain::speexdsp(48_000, frame_samples).expect("cadena SpeexDSP");
        assert_eq!(chain.added_latency_samples(), frame_samples);

        for block_index in 0..20 {
            let mut block = noise_block(frame_samples, 20 + block_index);
            assert!(chain.process(&mut block), "trama {frame_samples}");
        }

        let counters = chain.counters();
        assert_eq!(counters.stages_run, 20, "trama {frame_samples}");
        assert_eq!(counters.frames_bypassed, 0, "trama {frame_samples}");
        assert_eq!(counters.frames_rejected, 0, "trama {frame_samples}");
    }
}

#[cfg(feature = "speexdsp")]
#[test]
fn speexdsp_rejects_a_block_of_another_length() {
    let mut chain = NsChain::speexdsp(48_000, 240).expect("cadena SpeexDSP");
    let original = noise_block(120, 5);
    let mut block = original.clone();

    assert!(!chain.process(&mut block));
    assert_eq!(block, original, "un bloque rechazado no debe tocarse");
    let counters = chain.counters();
    assert_eq!(counters.stages_run, 0);
    assert_eq!(counters.frames_rejected, 1);
}
