//! Presupuesto por etapa y contadores idle/streaming.
//!
//! Las cifras del presupuesto son **propuestas**, no mediciones: estas pruebas
//! comprueban coherencia interna y que los contadores consumen las métricas
//! reales de jitter y drift sin duplicar el pipeline.

use arl_audio_core::budget::{
    self, IdleStreamBudget, Mode, Resource, Stage, ThreadClass, Verdict, STAGE_BUDGETS,
};
use arl_audio_core::drift::{DriftConfig, DriftController};
use arl_audio_core::frame::{AudioFrame, Flags, DATAGRAM_LEN};
use arl_audio_core::jitter::{JitterBuffer, JitterConfig, PopOutcome, PushOutcome};
use arl_audio_core::pcm16;

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
}

#[test]
fn memoria_preasignada_audio_separada_de_heap_control() {
    assert_eq!(budget::TRANSIENT_AUDIO_ALLOC_BYTES, 0);
    assert!(budget::AUDIO_PREALLOCATED_BYTES >= (budget::JITTER_SLOTS * DATAGRAM_LEN) as u64);
    let control_heap = budget::CONTROL_HEAP_LIMIT_BYTES;
    assert!(control_heap > 0, "el heap de control debe ser positivo");
    assert!(
        control_heap <= 1 << 20,
        "el heap de control no debe superar 1 MiB"
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
