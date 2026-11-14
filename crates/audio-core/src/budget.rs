//! Presupuesto de recursos y latencia por etapa, con contadores idle/streaming.
//!
//! ## Qué es y qué no es
//!
//! Todas las cifras de este módulo son **propuestas de presupuesto**, no
//! resultados: no hay baseline física (D-07) y esta crate no mide hardware. El
//! método de medida está declarado por recurso para que la medición física
//! posterior sea comparable; las pruebas sólo comprueban coherencia interna y
//! que los contadores leen las métricas reales de [`crate::jitter`] y
//! [`crate::drift`] sin duplicar el pipeline.
//!
//! ## Memoria
//!
//! El audio usa memoria preasignada y acotada ([`AUDIO_PREALLOCATED_BYTES`]);
//! el camino de consumo no asigna memoria transitoria
//! ([`TRANSIENT_AUDIO_ALLOC_BYTES`]). El heap del worker de control es un
//! presupuesto separado ([`CONTROL_HEAP_LIMIT_BYTES`]) y no comparte la cuenta
//! con el audio.
//!
//! ## Hilos Android
//!
//! Sólo se presupuestan los hilos propios de la app. ART, Compose y
//! RenderThread son externos al producto y se miden por separado; este módulo
//! no los suma ni los presenta como propios.

use crate::drift::DriftController;
use crate::frame::DATAGRAM_LEN;
use crate::jitter::CAPACITY_FRAMES;
use crate::metrics::{add, JitterCounters};

/// Etapa del camino de audio de extremo a extremo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Buffer de captura del dispositivo de origen.
    Capture,
    /// Formación del frame v1: 5 ms de audio PCM16LE.
    Frame,
    /// Ida por LAN hasta el receptor.
    Lan,
    /// Espera del target del jitter buffer en el receptor.
    Jitter,
    /// Resampling adaptativo y entrega al sink de salida.
    ResampleOutput,
}

/// Presupuesto propuesto de una etapa. `budget_us` es el objetivo y
/// `ceiling_us` el techo aceptable; ambos son propuestas, no mediciones.
#[derive(Clone, Copy, Debug)]
pub struct StageBudget {
    pub stage: Stage,
    pub budget_us: u32,
    pub ceiling_us: u32,
    /// Método/nota honesta sobre el origen de la cifra.
    pub note: &'static str,
}

/// Presupuesto propuesto por etapa, en microsegundos.
///
/// La prioridad es latencia LAN: la suma objetivo es 25 ms y el techo 80 ms.
/// Ninguno de los dos es una cifra lograda ni un benchmark; el target de
/// jitter por defecto (10 ms) y su máximo de contrato (40 ms) acotan esa fila.
pub const STAGE_BUDGETS: [StageBudget; 5] = [
    StageBudget {
        stage: Stage::Capture,
        budget_us: 5_000,
        ceiling_us: 12_000,
        note: "buffer de captura del dispositivo; no medido",
    },
    StageBudget {
        stage: Stage::Frame,
        budget_us: 5_000,
        ceiling_us: 5_000,
        note: "5 ms de audio por frame; duración fija del perfil v1",
    },
    StageBudget {
        stage: Stage::Lan,
        budget_us: 3_000,
        ceiling_us: 15_000,
        note: "ida LAN; prioridad del proyecto, sin cifras medidas",
    },
    StageBudget {
        stage: Stage::Jitter,
        budget_us: 10_000,
        ceiling_us: 40_000,
        note: "target por defecto del buffer; 40 ms es el máximo del contrato",
    },
    StageBudget {
        stage: Stage::ResampleOutput,
        budget_us: 2_000,
        ceiling_us: 8_000,
        note: "resample adaptativo y entrega al sink; no medido",
    },
];

/// Suma de los presupuestos objetivo, en microsegundos (propuesta).
pub const END_TO_END_BUDGET_US: u32 = 25_000;
/// Suma de los techos por etapa, en microsegundos (propuesta).
pub const END_TO_END_CEILING_US: u32 = 80_000;

/// Suma recalculada de [`STAGE_BUDGETS`].
pub fn total_budget_us() -> u32 {
    STAGE_BUDGETS.iter().map(|entry| entry.budget_us).sum()
}

/// Suma recalculada de los techos de [`STAGE_BUDGETS`].
pub fn total_ceiling_us() -> u32 {
    STAGE_BUDGETS.iter().map(|entry| entry.ceiling_us).sum()
}

/// Modo del proceso: en reposo o con audio en curso.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Idle,
    Streaming,
}

/// Acumuladores de un modo. Los incrementos saturan: un flujo hostil no puede
/// desbordar ni paniquear.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamCounters {
    /// Frames de red entregados al consumidor.
    pub frames: u64,
    /// Muestras correspondientes a esos frames.
    pub samples: u64,
    /// Huecos de 5 ms sin frame de red.
    pub gaps: u64,
    /// Huecos rellenados con PLC.
    pub plc_frames: u64,
    /// Frames descartados por cualquier causa del jitter buffer.
    pub dropped: u64,
    /// Resyncs duros del controlador de drift.
    pub resyncs: u64,
    /// Faltas de datos del sink de salida (contador propio del consumidor).
    pub underruns: u64,
    /// Excesos de datos del sink de salida (contador propio del consumidor).
    pub overruns: u64,
    baseline: JitterCounters,
    last_resyncs: u64,
}

impl StreamCounters {
    pub fn record_frames(&mut self, value: u64) {
        add(&mut self.frames, value);
    }

    pub fn record_samples(&mut self, value: u64) {
        add(&mut self.samples, value);
    }

    pub fn record_gap(&mut self) {
        add(&mut self.gaps, 1);
    }

    pub fn record_plc(&mut self) {
        add(&mut self.plc_frames, 1);
    }

    pub fn record_dropped(&mut self, value: u64) {
        add(&mut self.dropped, value);
    }

    pub fn record_resync(&mut self, value: u64) {
        add(&mut self.resyncs, value);
    }

    pub fn record_underrun(&mut self) {
        add(&mut self.underruns, 1);
    }

    pub fn record_overrun(&mut self) {
        add(&mut self.overruns, 1);
    }

    /// Consume una instantánea de los contadores del jitter buffer y acumula
    /// sólo el delta respecto de la lectura anterior. No reimplementa el
    /// pipeline: lee las métricas que el buffer ya produce.
    pub fn observe_jitter(&mut self, counters: &JitterCounters) {
        let last = self.baseline;
        let emitted = counters.emitted.saturating_sub(last.emitted);
        self.record_frames(emitted);
        self.record_samples(emitted.saturating_mul(crate::frame::SAMPLE_COUNT as u64));
        add(&mut self.gaps, counters.gaps.saturating_sub(last.gaps));
        add(
            &mut self.plc_frames,
            counters.plc_frames.saturating_sub(last.plc_frames),
        );
        add(
            &mut self.dropped,
            counters
                .dropped_total()
                .saturating_sub(last.dropped_total()),
        );
        self.baseline = *counters;
    }

    /// Consume el contador de resyncs del controlador de drift, también por
    /// delta sobre la lectura anterior.
    pub fn observe_drift(&mut self, drift: &DriftController) {
        let resyncs = drift.resyncs();
        add(&mut self.resyncs, resyncs.saturating_sub(self.last_resyncs));
        self.last_resyncs = resyncs;
    }
}

/// Contadores separados del proceso en reposo y en streaming.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdleStreamBudget {
    mode: Mode,
    /// Contadores del modo idle; no se mezclan con streaming.
    pub idle: StreamCounters,
    /// Contadores del modo streaming; no se mezclan con idle.
    pub streaming: StreamCounters,
}

impl IdleStreamBudget {
    /// Crea el presupuesto en modo idle.
    pub fn new() -> Self {
        Self {
            mode: Mode::Idle,
            idle: StreamCounters::default(),
            streaming: StreamCounters::default(),
        }
    }

    /// Modo vigente.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Cambia el modo; los contadores del otro modo se conservan.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Acumula las métricas reales de jitter y drift en el modo vigente.
    pub fn observe(&mut self, jitter: &JitterCounters, drift: &DriftController) {
        let target = match self.mode {
            Mode::Idle => &mut self.idle,
            Mode::Streaming => &mut self.streaming,
        };
        target.observe_jitter(jitter);
        target.observe_drift(drift);
    }
}

/// Recurso con techo propuesto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    RssIdle,
    RssStreaming,
    CpuIdleMillisPerSecond,
    CpuStreamingMillisPerSecond,
    ArtifactBytes,
    ThreadsOwn,
    ControlHeapBytes,
}

impl Resource {
    /// Todos los recursos presupuestados.
    pub const ALL: [Resource; 7] = [
        Resource::RssIdle,
        Resource::RssStreaming,
        Resource::CpuIdleMillisPerSecond,
        Resource::CpuStreamingMillisPerSecond,
        Resource::ArtifactBytes,
        Resource::ThreadsOwn,
        Resource::ControlHeapBytes,
    ];
}

/// Techo propuesto y método de medida de un recurso.
#[derive(Clone, Copy, Debug)]
pub struct ResourceCeiling {
    pub resource: Resource,
    pub ceiling: u64,
    pub unit: &'static str,
    pub method: &'static str,
}

/// Techos propuestos. `ThreadsOwn` cuenta sólo hilos propios; ART, Compose y
/// RenderThread se miden aparte y no están incluidos.
pub const RESOURCE_CEILINGS: [ResourceCeiling; 7] = [
    ResourceCeiling {
        resource: Resource::RssIdle,
        ceiling: 24 * 1024 * 1024,
        unit: "bytes",
        method: "VmRSS de /proc/<pid>/status en reposo",
    },
    ResourceCeiling {
        resource: Resource::RssStreaming,
        ceiling: 48 * 1024 * 1024,
        unit: "bytes",
        method: "VmRSS de /proc/<pid>/status con audio en curso",
    },
    ResourceCeiling {
        resource: Resource::CpuIdleMillisPerSecond,
        ceiling: 10,
        unit: "ms/s",
        method: "utime+stime de /proc/<pid>/stat por intervalo, en reposo",
    },
    ResourceCeiling {
        resource: Resource::CpuStreamingMillisPerSecond,
        ceiling: 150,
        unit: "ms/s",
        method: "utime+stime de /proc/<pid>/stat por intervalo, en streaming",
    },
    ResourceCeiling {
        resource: Resource::ArtifactBytes,
        ceiling: 8 * 1024 * 1024,
        unit: "bytes",
        method: "tamaño del binario release desplegado, no target/",
    },
    ResourceCeiling {
        resource: Resource::ThreadsOwn,
        ceiling: OWN_THREADS_PROPOSED as u64,
        unit: "hilos",
        method: "hilos propios por nombre/rol; ART/Compose/RenderThread aparte",
    },
    ResourceCeiling {
        resource: Resource::ControlHeapBytes,
        ceiling: CONTROL_HEAP_LIMIT_BYTES,
        unit: "bytes",
        method: "heap del worker de control; no incluye audio preasignado",
    },
];

/// Techo propuesto de `resource`.
pub fn proposed_ceiling(resource: Resource) -> u64 {
    RESOURCE_CEILINGS
        .iter()
        .find(|entry| entry.resource == resource)
        .map(|entry| entry.ceiling)
        .unwrap_or(0)
}

/// Método de medida declarado para `resource`.
pub fn measurement_method(resource: Resource) -> &'static str {
    RESOURCE_CEILINGS
        .iter()
        .find(|entry| entry.resource == resource)
        .map(|entry| entry.method)
        .unwrap_or("")
}

/// Resultado de comparar una observación con su techo propuesto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Within,
    Over { observed: u64, ceiling: u64 },
}

/// Compara una observación con el techo propuesto.
pub fn check(resource: Resource, observed: u64) -> Verdict {
    let ceiling = proposed_ceiling(resource);
    if observed > ceiling {
        Verdict::Over { observed, ceiling }
    } else {
        Verdict::Within
    }
}

/// Slots del jitter buffer que forman la memoria de audio preasignada.
pub const JITTER_SLOTS: usize = CAPACITY_FRAMES;

/// Memoria de audio preasignada: peor caso de los slots del jitter buffer
/// (508 B por frame, cota superior) más el último frame para PLC.
pub const AUDIO_PREALLOCATED_BYTES: u64 =
    (JITTER_SLOTS * DATAGRAM_LEN) as u64 + core::mem::size_of::<crate::pcm16::Samples>() as u64;

/// Asignación transitoria permitida en el camino de audio: ninguna.
pub const TRANSIENT_AUDIO_ALLOC_BYTES: u64 = 0;

/// Presupuesto separado del heap del worker de control (64 KiB), sin contar
/// la memoria preasignada de audio.
pub const CONTROL_HEAP_LIMIT_BYTES: u64 = 64 * 1024;

/// Hilos propios presupuestados: captura, red, consumidor de jitter y worker
/// de control.
pub const OWN_THREADS_PROPOSED: u8 = 4;

/// Clase de hilo para separar lo propio de lo medido por el sistema Android.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadClass {
    Capture,
    Network,
    JitterConsumer,
    ControlWorker,
    Art,
    Compose,
    RenderThread,
}

const OWN_THREAD_CLASSES: [ThreadClass; 4] = [
    ThreadClass::Capture,
    ThreadClass::Network,
    ThreadClass::JitterConsumer,
    ThreadClass::ControlWorker,
];

/// Hilos propios presupuestados, en orden.
pub fn own_thread_classes() -> [ThreadClass; 4] {
    OWN_THREAD_CLASSES
}

/// `true` si la clase es un hilo propio de la app.
pub fn is_own_thread(class: ThreadClass) -> bool {
    matches!(
        class,
        ThreadClass::Capture
            | ThreadClass::Network
            | ThreadClass::JitterConsumer
            | ThreadClass::ControlWorker
    )
}
