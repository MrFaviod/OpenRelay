//! Presupuesto por etapa y contadores idle/streaming.
//!
//! Las cifras del presupuesto son **propuestas**, no mediciones: estas pruebas
//! comprueban coherencia interna y que los contadores consumen las métricas
//! reales de jitter y drift sin duplicar el pipeline.

use openrelay_audio_core::budget::{
    self, IdleStreamBudget, Mode, Resource, Stage, ThreadClass, Verdict, STAGE_BUDGETS,
};
use openrelay_audio_core::drift::{DriftConfig, DriftController};
use openrelay_audio_core::frame::{AudioFrame, Flags, DATAGRAM_LEN};
use openrelay_audio_core::jitter::{JitterBuffer, JitterConfig, PopOutcome, PushOutcome};
use openrelay_audio_core::pcm16;
use openrelay_audio_core::resample::LinearResampler;

fn frame(seq: u32) -> AudioFrame {
    AudioFrame::new(1, seq, Flags::NONE, pcm16::SILENCE).expect("frame válido")
}

#[test]
fn presupuesto_por_etapa_suma_y_respeta_techos() {
    assert_eq!(STAGE_BUDGETS.len(), 5);
    let mut total_budget = 0u32;
    let mut total_ceiling = 0u32;
    for entry in &STAGE_BUDGETS {
        assert!(entry.budget_us > 0, "{:?} sin presupuesto", entry.stage);
        assert!(
            entry.budget_us <= entry.ceiling_us,
            "{:?} con presupuesto mayor que su techo",
            entry.stage
        );
        assert!(
            !entry.note.is_empty(),
            "{:?} sin método declarado",
            entry.stage
        );
        total_budget += entry.budget_us;
        total_ceiling += entry.ceiling_us;
    }
    assert_eq!(total_budget, budget::total_budget_us());
    assert_eq!(total_ceiling, budget::total_ceiling_us());
    assert_eq!(total_budget, budget::END_TO_END_BUDGET_US);
    assert_eq!(total_ceiling, budget::END_TO_END_CEILING_US);
    assert!(total_budget < total_ceiling);
}

#[test]
fn etapas_cubren_el_camino_y_son_unicas() {
    let stages: Vec<Stage> = STAGE_BUDGETS.iter().map(|entry| entry.stage).collect();
    for expected in [
        Stage::Capture,
        Stage::Frame,
        Stage::Lan,
        Stage::Jitter,
        Stage::ResampleOutput,
    ] {
        assert_eq!(
            stages.iter().filter(|stage| **stage == expected).count(),
            1,
            "etapa {expected:?} ausente o duplicada"
        );
    }
    for (i, a) in stages.iter().enumerate() {
        for b in &stages[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn idle_y_streaming_estan_separados_y_saturan() {
    let mut counters = IdleStreamBudget::new();
    counters.idle.record_frames(u64::MAX);
    counters.idle.record_frames(1);
    assert_eq!(counters.idle.frames, u64::MAX, "satura en vez de desbordar");
    assert_eq!(counters.streaming.frames, 0, "idle no contamina streaming");

    counters.set_mode(Mode::Streaming);
    counters.streaming.record_frames(1);
    assert_eq!(counters.streaming.frames, 1);
    assert_eq!(
        counters.idle.frames,
        u64::MAX,
        "streaming no contamina idle"
    );
    assert_eq!(counters.mode(), Mode::Streaming);
}

#[test]
fn contadores_consumen_metricas_reales_de_jitter_y_drift() {
    let mut buffer = JitterBuffer::new(JitterConfig::new(5));
    assert_eq!(buffer.push(frame(0), 0), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(5_000), PopOutcome::Emitted(_)));
    // Sin frame para el siguiente deadline: hueco rellenado con PLC.
    assert!(matches!(buffer.pop(10_000), PopOutcome::Emitted(_)));
    let jitter = buffer.counters();
    assert_eq!(jitter.emitted, 1);
    assert_eq!(jitter.gaps, 1);
    assert_eq!(jitter.plc_frames, 1);

    let mut drift = DriftController::new(DriftConfig::default()).expect("config válida");
    drift.observe(0, 4, 0, 0).expect("primera observación");
    let update = drift
        .observe(1_000_000, 100, 48_000, 48_000)
        .expect("segunda observación");
    assert!(update.hard_resync);
    assert_eq!(drift.resyncs(), 1);

    let mut counters = IdleStreamBudget::new();
    counters.set_mode(Mode::Streaming);
    counters.observe(&jitter, &drift);
    let snapshot = counters.streaming;
    assert_eq!(snapshot.frames, jitter.emitted);
    assert_eq!(snapshot.gaps, jitter.gaps);
    assert_eq!(snapshot.plc_frames, jitter.plc_frames);
    assert_eq!(snapshot.resyncs, drift.resyncs());
    assert_eq!(snapshot.dropped, jitter.dropped_total());
    assert_eq!(counters.idle.frames, 0, "modo idle intacto");

    // Segunda lectura sin cambios: el delta es cero, no se duplica el pipeline.
    counters.observe(&jitter, &drift);
    assert_eq!(counters.streaming.frames, snapshot.frames);
    assert_eq!(counters.streaming.resyncs, snapshot.resyncs);

    // Volver a idle con la misma instantánea tampoco reatribuye ni duplica:
    // la línea base del pipeline ya está sincronizada por la observación previa.
    counters.set_mode(Mode::Idle);
    counters.observe(&jitter, &drift);
    assert_eq!(
        counters.idle.frames, 0,
        "idle no hereda emisiones de streaming"
    );
    assert_eq!(counters.idle.resyncs, 0, "idle no hereda resyncs");
    assert_eq!(counters.streaming, snapshot, "streaming queda intacto");
}

#[test]
fn alternar_streaming_idle_streaming_no_duplica_emisiones() {
    let mut buffer = JitterBuffer::new(JitterConfig::new(5));
    let drift = DriftController::new(DriftConfig::default()).expect("config válida");
    let mut counters = IdleStreamBudget::new();

    // Emisión 1 en streaming.
    assert_eq!(buffer.push(frame(0), 0), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(5_000), PopOutcome::Emitted(_)));
    counters.set_mode(Mode::Streaming);
    counters.observe(&buffer.counters(), &drift);
    assert_eq!(counters.streaming.frames, 1);
    assert_eq!(counters.idle.frames, 0, "idle arranca en cero");

    // Emisión 2 en idle: se cuenta en idle y no se reatribuye al volver.
    assert_eq!(buffer.push(frame(1), 6_000), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(10_000), PopOutcome::Emitted(_)));
    counters.set_mode(Mode::Idle);
    counters.observe(&buffer.counters(), &drift);
    assert_eq!(counters.idle.frames, 1);
    assert_eq!(counters.streaming.frames, 1, "idle no contamina streaming");

    // Emisión 3 de vuelta en streaming: sólo suma la nueva, no las dos previas.
    assert_eq!(buffer.push(frame(2), 11_000), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(15_000), PopOutcome::Emitted(_)));
    counters.set_mode(Mode::Streaming);
    counters.observe(&buffer.counters(), &drift);
    assert_eq!(buffer.counters().emitted, 3);
    assert_eq!(
        counters.streaming.frames, 2,
        "streaming cuenta las emisiones 1 y 3, no 1, 2 y 3"
    );
    assert_eq!(counters.idle.frames, 1);
    assert_eq!(counters.streaming.frames + counters.idle.frames, 3);
    assert_eq!(
        counters.streaming.samples + counters.idle.samples,
        3 * 240,
        "cada emisión aporta sus 240 muestras una sola vez"
    );

    // Hueco rellenado con PLC en streaming: se cuenta una vez y no pasa a idle.
    assert!(matches!(buffer.pop(20_000), PopOutcome::Emitted(_)));
    let with_gap = buffer.counters();
    assert_eq!(with_gap.gaps, 1);
    assert_eq!(with_gap.plc_frames, 1);
    counters.observe(&with_gap, &drift);
    assert_eq!(
        counters.streaming.gaps, 1,
        "el hueco se cuenta en streaming"
    );
    assert_eq!(counters.streaming.plc_frames, 1);
    counters.set_mode(Mode::Idle);
    counters.observe(&with_gap, &drift);
    assert_eq!(counters.idle.gaps, 0, "idle no duplica el hueco");
    assert_eq!(counters.idle.plc_frames, 0);
    assert_eq!(counters.streaming.frames, 2, "sin emisiones nuevas no suma");
}

#[test]
fn observaciones_repetidas_suman_cero_y_el_reinicio_cuenta_desde_cero() {
    let mut buffer = JitterBuffer::new(JitterConfig::new(5));
    assert_eq!(buffer.push(frame(0), 0), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(5_000), PopOutcome::Emitted(_)));
    assert_eq!(buffer.push(frame(1), 6_000), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(10_000), PopOutcome::Emitted(_)));
    assert_eq!(buffer.push(frame(2), 11_000), PushOutcome::Accepted);
    assert!(matches!(buffer.pop(15_000), PopOutcome::Emitted(_)));
    let drift = DriftController::new(DriftConfig::default()).expect("config válida");
    let mut counters = IdleStreamBudget::new();
    counters.set_mode(Mode::Streaming);
    counters.observe(&buffer.counters(), &drift);
    let once = counters.streaming;
    assert_eq!(once.frames, 3);

    // Repetir la misma instantánea no cambia ningún campo.
    counters.observe(&buffer.counters(), &drift);
    assert_eq!(counters.streaming, once, "delta cero no duplica");

    // Pipeline nuevo con contadores por debajo de la línea base: se cuenta lo
    // nuevo desde cero en vez de saturar el delta a cero y perderlo.
    let mut fresh = JitterBuffer::new(JitterConfig::new(5));
    assert_eq!(fresh.push(frame(0), 0), PushOutcome::Accepted);
    assert!(matches!(fresh.pop(5_000), PopOutcome::Emitted(_)));
    counters.observe(&fresh.counters(), &drift);
    assert_eq!(
        counters.streaming.frames,
        once.frames + 1,
        "un reinicio del pipeline se cuenta desde cero"
    );
    assert_eq!(counters.streaming.samples, once.samples + 240);
}

#[test]
fn memoria_preasignada_usa_size_of_real_y_no_el_wire() {
    use core::mem::size_of;

    assert_eq!(budget::FRAME_WIRE_BYTES, DATAGRAM_LEN as u64);
    assert_eq!(budget::AUDIO_FRAME_BYTES, size_of::<AudioFrame>() as u64);
    assert_ne!(
        budget::AUDIO_FRAME_BYTES,
        budget::FRAME_WIRE_BYTES,
        "el frame Rust no mide lo mismo que el datagrama wire de 508 B"
    );
    assert_eq!(
        budget::AUDIO_PREALLOCATED_BYTES,
        size_of::<JitterBuffer>() as u64,
        "la memoria preasignada es size_of::<JitterBuffer>(), no slots x 508 B"
    );
    assert_eq!(
        budget::AUDIO_PATH_STATE_BYTES,
        (size_of::<JitterBuffer>() + size_of::<LinearResampler>() + size_of::<DriftController>())
            as u64,
        "el total del camino de audio suma los tres estados inline reales"
    );
    assert_eq!(
        budget::AUDIO_PATH_STATE_BYTES - budget::AUDIO_PREALLOCATED_BYTES,
        budget::RESAMPLER_STATE_BYTES + budget::DRIFT_STATE_BYTES,
        "resampler y drift se declaran aparte, no dentro del jitter buffer"
    );
    assert_eq!(budget::TRANSIENT_AUDIO_ALLOC_BYTES, 0);
    assert_eq!(budget::JITTER_SLOTS, 16, "capacidad de la cola, no bytes");

    let control_heap = budget::CONTROL_HEAP_LIMIT_BYTES;
    assert_eq!(
        control_heap,
        64 * 1024,
        "el heap de control se declara en 64 KiB, por debajo de 1 MiB"
    );
    assert_ne!(
        budget::AUDIO_PREALLOCATED_BYTES,
        budget::CONTROL_HEAP_LIMIT_BYTES,
        "audio preasignado y heap de control son presupuestos distintos"
    );
}

#[test]
fn hilos_propios_excluyen_art_compose_y_render_thread() {
    let own = budget::own_thread_classes();
    assert_eq!(own.len(), usize::from(budget::OWN_THREADS_PROPOSED));
    for class in own {
        assert!(budget::is_own_thread(class), "{class:?} debía ser propio");
    }
    for external in [
        ThreadClass::Art,
        ThreadClass::Compose,
        ThreadClass::RenderThread,
    ] {
        assert!(!budget::is_own_thread(external));
        assert!(!own.contains(&external), "{external:?} no es hilo propio");
    }
}

#[test]
fn techos_propuestos_marcan_exceso() {
    for resource in Resource::ALL {
        assert!(budget::proposed_ceiling(resource) > 0);
        assert!(!budget::measurement_method(resource).is_empty());
        assert_eq!(
            budget::check(resource, budget::proposed_ceiling(resource)),
            Verdict::Within
        );
        assert!(matches!(
            budget::check(resource, budget::proposed_ceiling(resource) + 1),
            Verdict::Over { .. }
        ));
    }
}
