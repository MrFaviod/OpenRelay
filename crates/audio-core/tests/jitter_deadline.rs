//! T06 — jitter acotado, descarte por deadline, huecos/PLC y contadores.
//!
//! Los tests usan un reloj monotónico local explícito en microsegundos: el
//! buffer nunca compara tiempos de dispositivos distintos, sólo diferencias
//! del mismo reloj inyectado por el test.

use arl_audio_core::jitter::{
    EmitKind, JitterBuffer, JitterConfig, JitterEmit, PopOutcome, PushOutcome, CAPACITY_FRAMES,
    TARGET_DEFAULT_MS, TARGET_MAX_MS, TARGET_MIN_MS,
};
use arl_audio_core::metrics::{JitterCounters, JitterMetrics};
use arl_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};

const MS: u64 = 1_000;

fn frame(epoch: u32, seq: u32, marker: i16) -> AudioFrame {
    let mut samples = [0i16; SAMPLE_COUNT];
    samples[0] = marker;
    AudioFrame::new(epoch, seq, Flags::NONE, samples).expect("frame de prueba")
}

fn flagged(epoch: u32, seq: u32, flags: Flags, marker: i16) -> AudioFrame {
    let mut samples = [0i16; SAMPLE_COUNT];
    samples[0] = marker;
    AudioFrame::new(epoch, seq, flags, samples).expect("frame de prueba")
}

fn emitted(outcome: PopOutcome) -> JitterEmit {
    match outcome {
        PopOutcome::Emitted(emit) => emit,
        PopOutcome::NotDue => panic!("se esperaba una emisión y llegó NotDue"),
    }
}

fn assert_not_due(outcome: PopOutcome) {
    assert!(
        matches!(outcome, PopOutcome::NotDue),
        "se esperaba NotDue, llegó {outcome:?}"
    );
}

#[test]
fn target_se_limita_a_la_ventana_5_40() {
    assert_eq!(TARGET_MIN_MS, 5);
    assert_eq!(TARGET_MAX_MS, 40);
    assert_eq!(TARGET_DEFAULT_MS, 10);
    assert_eq!(JitterConfig::default().target_ms(), 10);
    assert_eq!(JitterConfig::new(0).target_ms(), 5);
    assert_eq!(JitterConfig::new(5).target_ms(), 5);
    assert_eq!(JitterConfig::new(40).target_ms(), 40);
    assert_eq!(JitterConfig::new(41).target_ms(), 40);
    assert_eq!(JitterConfig::new(u16::MAX).target_ms(), 40);
}

#[test]
fn prefill_y_deadline_en_rejilla_fija() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    assert_eq!(jb.push(frame(1, 0, 7), 0), PushOutcome::Accepted);

    assert_not_due(jb.pop(9_999));
    let first = emitted(jb.pop(10_000));
    assert_eq!(first.kind, EmitKind::Network);
    assert_eq!(first.frame.seq(), 0);
    assert_eq!(first.frame.samples()[0], 7);
    assert_eq!(jb.deadline_us(), 15_000);

    assert_not_due(jb.pop(14_999));
    let second = emitted(jb.pop(15_000));
    assert_eq!(second.kind, EmitKind::Plc, "hueco PLC tras el primer frame");
    assert_eq!(second.frame.seq(), 1);
    assert_eq!(jb.counters().gaps, 1);
}

#[test]
fn deadline_no_se_pospone_por_llegadas_nuevas() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    // La cola arranca llena y sigue recibiendo una llegada entre cada deadline.
    for i in 0..CAPACITY_FRAMES as u32 {
        let _ = jb.push(frame(1, i, i as i16), 0);
    }
    assert!(jb.peak_occupancy() <= CAPACITY_FRAMES);

    let mut now = 10 * MS;
    for expected_seq in 0..8u32 {
        // Llegada nueva 1 ms antes del deadline: no debe mover la rejilla.
        let _ = jb.push(frame(1, CAPACITY_FRAMES as u32 + expected_seq, 0), now - MS);
        let emit = emitted(jb.pop(now));
        assert_eq!(emit.kind, EmitKind::Network);
        assert_eq!(emit.frame.seq(), expected_seq);
        now += 5 * MS;
    }
    assert_eq!(jb.deadline_us(), 10 * MS + 8 * 5 * MS);
}

#[test]
fn frame_tardio_se_descarta() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(1, 0, 0), 0);
    let _ = jb.push(frame(1, 1, 1), 5 * MS);
    let _ = jb.push(frame(1, 2, 2), 10 * MS);

    assert_eq!(emitted(jb.pop(10 * MS)).frame.seq(), 0);
    assert_eq!(emitted(jb.pop(15 * MS)).frame.seq(), 1);
    assert_eq!(emitted(jb.pop(20 * MS)).frame.seq(), 2);

    let late = jb.push(frame(1, 0, 9), 21 * MS);
    assert_eq!(late, PushOutcome::Late);
    assert_eq!(jb.counters().dropped_late, 1);
    assert_eq!(jb.counters().emitted, 3);
}

#[test]
fn duplicado_en_cola_se_descarta() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    assert_eq!(jb.push(frame(1, 3, 1), 0), PushOutcome::Accepted);
    assert_eq!(jb.push(frame(1, 3, 2), MS), PushOutcome::Duplicate);
    assert_eq!(jb.counters().dropped_duplicate, 1);
    assert_eq!(jb.occupancy(), 1);

    let emit = emitted(jb.pop(10 * MS));
    assert_eq!(emit.frame.seq(), 3);
    assert_eq!(emit.frame.samples()[0], 1, "se conserva la primera copia");
}

#[test]
fn reorden_de_20_ms_se_acepta_y_se_emite_en_orden() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let arrivals = [
        (0u32, 0u64),
        (1, 5 * MS),
        (2, 10 * MS),
        (4, 15 * MS),
        (5, 20 * MS),
        (3, 25 * MS), // 20 ms tarde respecto de su vecino 4
    ];
    for (seq, at) in arrivals {
        let outcome = jb.push(frame(1, seq, seq as i16), at);
        if seq == 3 {
            assert_eq!(outcome, PushOutcome::Reordered);
        } else {
            assert_eq!(outcome, PushOutcome::Accepted);
        }
    }
    assert_eq!(jb.counters().reordered, 1);
    assert_eq!(jb.occupancy(), 6);

    for seq in 0..6u32 {
        let emit = emitted(jb.pop(30 * MS + u64::from(seq) * 5 * MS));
        assert_eq!(emit.kind, EmitKind::Network);
        assert_eq!(emit.frame.seq(), seq);
    }
}

#[test]
fn hueco_genera_plc_y_luego_silencio() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(1, 0, 1_000), 0);
    let _ = jb.push(frame(1, 5, 2_000), 0);

    assert_eq!(emitted(jb.pop(10 * MS)).frame.seq(), 0);
    let plc1 = emitted(jb.pop(15 * MS));
    assert_eq!(plc1.kind, EmitKind::Plc);
    assert_eq!(plc1.frame.seq(), 1);
    let plc2 = emitted(jb.pop(20 * MS));
    assert_eq!(plc2.kind, EmitKind::Plc);
    let plc3 = emitted(jb.pop(25 * MS));
    assert_eq!(plc3.kind, EmitKind::Plc);
    let silence = emitted(jb.pop(30 * MS));
    assert_eq!(silence.kind, EmitKind::Silence);
    assert!(silence.frame.flags().is_silence());

    assert_eq!(jb.counters().gaps, 4);
    assert_eq!(jb.counters().plc_frames, 3);
    assert_eq!(jb.counters().silence_frames, 1);

    // La red vuelve: el PLC se reinicia y el frame real se emite.
    let _ = jb.push(frame(1, 5, 2_000), 30 * MS);
    let resumed = emitted(jb.pop(35 * MS));
    assert_eq!(resumed.kind, EmitKind::Network);
    assert_eq!(resumed.frame.seq(), 5);
    assert_eq!(resumed.frame.samples()[0], 2_000);
}

#[test]
fn wrap_de_seq_u32_no_pierde_frames() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let start = u32::MAX - 1;
    let seqs = [start, u32::MAX, 0, 1];
    for (i, seq) in seqs.iter().enumerate() {
        let outcome = jb.push(frame(1, *seq, i as i16), i as u64 * 5 * MS);
        assert_eq!(outcome, PushOutcome::Accepted, "seq {seq} tras el wrap");
    }
    for (i, seq) in seqs.iter().enumerate() {
        let emit = emitted(jb.pop(10 * MS + i as u64 * 5 * MS));
        assert_eq!(emit.frame.seq(), *seq);
    }
    assert_eq!(jb.counters().dropped_late, 0);
    assert_eq!(jb.counters().dropped_overflow, 0);
}

#[test]
fn epoca_vieja_con_discontinuity_no_resetea() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(5, 0, 1), 0);
    let _ = jb.push(frame(5, 1, 2), 5 * MS);
    let _ = jb.push(frame(5, 2, 3), 10 * MS);
    assert_eq!(emitted(jb.pop(10 * MS)).frame.seq(), 0);

    let stale = jb.push(flagged(4, 99, Flags::DISCONTINUITY, 9), 11 * MS);
    assert_eq!(stale, PushOutcome::OldEpoch);
    assert_eq!(jb.counters().dropped_old_epoch, 1);
    assert_eq!(jb.counters().resets, 0);

    assert_eq!(emitted(jb.pop(15 * MS)).frame.seq(), 1);
    assert_eq!(emitted(jb.pop(20 * MS)).frame.seq(), 2);
}

#[test]
fn epoca_nueva_resetea_y_reancla_deadline() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(5, 0, 1), 0);
    let _ = jb.push(frame(5, 1, 2), 5 * MS);
    assert_eq!(emitted(jb.pop(10 * MS)).frame.seq(), 0);

    let outcome = jb.push(flagged(6, 100, Flags::DISCONTINUITY, 7), 12 * MS);
    assert_eq!(outcome, PushOutcome::NewEpoch);
    assert_eq!(jb.counters().resets, 1);
    assert_eq!(jb.occupancy(), 1, "la cola de la época vieja se descarta");

    assert_not_due(jb.pop(21_999));
    let emit = emitted(jb.pop(22_000));
    assert_eq!(emit.frame.epoch(), 6);
    assert_eq!(emit.frame.seq(), 100);
}

#[test]
fn discontinuity_con_salto_mayor_que_la_capacidad_resincroniza() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(1, 0, 1), 0);
    assert_eq!(emitted(jb.pop(10 * MS)).frame.seq(), 0);

    let outcome = jb.push(flagged(1, 1_000, Flags::DISCONTINUITY, 2), 100 * MS);
    assert_eq!(outcome, PushOutcome::Resynced);
    assert_eq!(jb.counters().resets, 1);
    assert_not_due(jb.pop(109_999));
    let emit = emitted(jb.pop(110_000));
    assert_eq!(emit.frame.seq(), 1_000);
}

#[test]
fn flood_no_crece_la_cola_ni_paniquea() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    for i in 0..10_000u32 {
        let _ = jb.push(frame(1, i, 1), u64::from(i) * 100);
    }
    assert!(jb.occupancy() <= CAPACITY_FRAMES);
    assert!(jb.peak_occupancy() <= CAPACITY_FRAMES);
    assert!(jb.counters().dropped_overflow > 0);
    assert_eq!(jb.counters().accepted, CAPACITY_FRAMES as u64);
}

#[test]
fn reloj_local_que_retrocede_no_paniquea() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(1, 0, 1), 100_000);
    assert_not_due(jb.pop(50_000));
    assert_eq!(jb.counters().clock_regressions, 1);
    assert_not_due(jb.pop(99_999));
    assert_eq!(jb.counters().clock_regressions, 2);
    assert_eq!(emitted(jb.pop(110_000)).frame.seq(), 0);
}

#[test]
fn seq_y_epoca_extremos_no_paniquean() {
    let extreme = AudioFrame::new(u32::MAX, u32::MAX, Flags::NONE, [0i16; SAMPLE_COUNT])
        .expect("época máxima válida");
    let mut jb = JitterBuffer::new(JitterConfig::new(u16::MAX));
    assert_eq!(jb.push(extreme, u64::MAX / 2), PushOutcome::Accepted);
    let emit = emitted(jb.pop(u64::MAX / 2 + 40 * MS));
    assert_eq!(emit.frame.epoch(), u32::MAX);
    assert_eq!(emit.frame.seq(), u32::MAX);
    assert_eq!(jb.target_ms(), TARGET_MAX_MS);
}

#[test]
fn metricas_de_jitter_de_llegada_se_registran() {
    let mut jb = JitterBuffer::new(JitterConfig::default());
    let _ = jb.push(frame(1, 0, 1), 0);
    let _ = jb.push(frame(1, 1, 2), 12 * MS); // 7 ms de desviación sobre 5 ms
    let m: JitterMetrics = jb.metrics();
    assert_eq!(m.arrival_jitter_samples, 1);
    assert!(m.arrival_jitter_us_max >= 7 * MS);
    assert!(m.mean_us() >= 7 * MS);
}

#[test]
fn contadores_saturan_sin_overflow() {
    let mut counters = JitterCounters {
        dropped_late: u64::MAX,
        ..JitterCounters::default()
    };
    counters.record_dropped_late();
    assert_eq!(counters.dropped_late, u64::MAX);

    let mut metrics = JitterMetrics {
        arrival_jitter_us_sum: u64::MAX,
        ..JitterMetrics::default()
    };
    metrics.record(u64::MAX);
    assert_eq!(metrics.arrival_jitter_us_sum, u64::MAX);
    assert_eq!(metrics.arrival_jitter_us_max, u64::MAX);
}
