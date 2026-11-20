//! Presupuesto de recursos y latencia por etapa, con contadores idle/streaming.
//!
//! ## Qué es y qué no es
//!
//! Todas las cifras de este módulo son **propuestas de presupuesto**, no
//! resultados: no hay baseline física y esta crate no mide hardware. El
//! método de medida está declarado por recurso para que la medición física
//! posterior sea comparable; las pruebas sólo comprueban coherencia interna y
//! que los contadores leen las métricas reales de [`crate::jitter`] y
//! [`crate::drift`] sin duplicar el pipeline.
//!
//! ## Memoria
//!
//! El audio usa memoria preasignada y acotada ([`AUDIO_PREALLOCATED_BYTES`],
//! derivada de `size_of::<JitterBuffer>()`); el camino de consumo no asigna
//! memoria transitoria ([`TRANSIENT_AUDIO_ALLOC_BYTES`]). El estado inline del
//! resampler y del controlador de drift se declara aparte
//! ([`AUDIO_PATH_STATE_BYTES`]). El heap del worker de control es un
//! presupuesto separado ([`CONTROL_HEAP_LIMIT_BYTES`]) y no comparte la cuenta
//! con el audio. Ninguna de estas cifras es RSS ni tamaño wire.
//!
//! ## Hilos Android
//!
//! Sólo se presupuestan los hilos propios de la app. ART, Compose y
//! RenderThread son externos al producto y se miden por separado; este módulo
//! no los suma ni los presenta como propios.

use crate::drift::DriftController;
use crate::frame::{AudioFrame, DATAGRAM_LEN};
use crate::jitter::{JitterBuffer, CAPACITY_FRAMES};
use crate::metrics::{add, JitterCounters};
use crate::resample::LinearResampler;

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
        note: "5 ms of audio per frame; fixed duration of the v1 profile",
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
        note: "default jitter-buffer target; 40 ms is the contract maximum",
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

/// Delta de un contador que sólo crece. Si el valor actual es menor que el
/// anterior, el pipeline se reinició (buffer o controlador nuevos) y se cuenta
/// desde cero: el reinicio no pierde lo nuevo ni resta lo ya acumulado.
fn delta(current: u64, last: u64) -> u64 {
    if current >= last {
        current - last
    } else {
        current
    }
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
    /// pipeline: lee las métricas que el buffer ya produce. Cada delta se
    /// contabiliza una sola vez; una lectura repetida sin cambios suma cero.
    pub fn observe_jitter(&mut self, counters: &JitterCounters) {
        let last = self.baseline;
        let emitted = delta(counters.emitted, last.emitted);
        self.record_frames(emitted);
        self.record_samples(emitted.saturating_mul(crate::frame::SAMPLE_COUNT as u64));
        add(&mut self.gaps, delta(counters.gaps, last.gaps));
        add(
            &mut self.plc_frames,
            delta(counters.plc_frames, last.plc_frames),
        );
        let dropped = delta(counters.dropped_late, last.dropped_late)
            .saturating_add(delta(counters.dropped_duplicate, last.dropped_duplicate))
            .saturating_add(delta(counters.dropped_old_epoch, last.dropped_old_epoch))
            .saturating_add(delta(counters.dropped_overflow, last.dropped_overflow))
            .saturating_add(delta(counters.dropped_invalid, last.dropped_invalid));
        add(&mut self.dropped, dropped);
        self.baseline = *counters;
    }

    /// Consume el contador de resyncs del controlador de drift, también por
    /// delta sobre la lectura anterior.
    pub fn observe_drift(&mut self, drift: &DriftController) {
        let resyncs = drift.resyncs();
        add(&mut self.resyncs, delta(resyncs, self.last_resyncs));
        self.last_resyncs = resyncs;
    }

    /// Avanza la línea base sin acumular. Lo usa el modo inactivo en cada
    /// observación para que un cambio de modo no reatribuya métricas que ya
    /// contó el otro modo.
    fn rebase(&mut self, counters: &JitterCounters, drift: &DriftController) {
        self.baseline = *counters;
        self.last_resyncs = drift.resyncs();
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

    /// Cambia el modo; los contadores del otro modo se conservan. La línea base
    /// del pipeline es compartida: cada observación la avanza para ambos modos,
    /// así que volver a un modo no reatribuye lo ya contado. El intervalo que
    /// quede entre la última observación y el cambio se atribuye al modo
    /// vigente en la siguiente observación; observa antes de cambiar si el
    /// límite importa.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Acumula las métricas reales de jitter y drift en el modo vigente y
    /// sincroniza la línea base del otro modo sin sumarle nada. Así cada delta
    /// del pipeline se cuenta exactamente una vez, en el modo activo cuando se
    /// observó, y alternar streaming→idle→streaming no duplica emisiones.
    pub fn observe(&mut self, jitter: &JitterCounters, drift: &DriftController) {
        match self.mode {
            Mode::Idle => {
                self.idle.observe_jitter(jitter);
                self.idle.observe_drift(drift);
                self.streaming.rebase(jitter, drift);
            }
            Mode::Streaming => {
                self.streaming.observe_jitter(jitter);
                self.streaming.observe_drift(drift);
                self.idle.rebase(jitter, drift);
            }
        }
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
        method: "size of the deployed release binary, not target/",
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

/// Slots del jitter buffer (capacidad de la cola, no bytes).
pub const JITTER_SLOTS: usize = CAPACITY_FRAMES;

/// Bytes wire de un frame v1: cabecera de contrato más payload PCM16.
///
/// No es el tamaño en memoria de [`AudioFrame`]: el tipo Rust guarda una
/// cabecera propia y cada slot añade el tag de `Option`, así que la memoria
/// real se declara con `size_of`, no multiplicando 508 B por slot.
pub const FRAME_WIRE_BYTES: u64 = DATAGRAM_LEN as u64;

/// Tamaño real en memoria Rust de un frame, cabecera validada incluida.
pub const AUDIO_FRAME_BYTES: u64 = core::mem::size_of::<AudioFrame>() as u64;

/// Tamaño real en memoria Rust del jitter buffer completo: slots, último frame
/// para PLC y contabilidad. Derivado de `size_of`, no del tamaño wire.
pub const JITTER_BUFFER_BYTES: u64 = core::mem::size_of::<JitterBuffer>() as u64;

/// Tamaño real del estado inline del resampler (anillo y fase entre frames).
pub const RESAMPLER_STATE_BYTES: u64 = core::mem::size_of::<LinearResampler>() as u64;

/// Tamaño real del estado inline del controlador de drift.
pub const DRIFT_STATE_BYTES: u64 = core::mem::size_of::<DriftController>() as u64;

/// Memoria de audio preasignada: el jitter buffer completo tal como lo reserva
/// `size_of::<JitterBuffer>()` (slots, PLC y contabilidad), no 16 x 508 B.
///
/// No incluye el estado del resampler ([`RESAMPLER_STATE_BYTES`]) ni el del
/// controlador de drift ([`DRIFT_STATE_BYTES`]): son estados inline separados
/// del camino de audio y se declaran aparte en [`AUDIO_PATH_STATE_BYTES`]. No
/// incluye heap, metadatos del allocador, pilas de hilos ni RSS.
pub const AUDIO_PREALLOCATED_BYTES: u64 = JITTER_BUFFER_BYTES;

/// Estado inline completo del camino de audio: jitter buffer más resampler y
/// controlador de drift. Sigue siendo una suma de `size_of` en compilación, no
/// una medición de RSS ni un presupuesto mágico.
pub const AUDIO_PATH_STATE_BYTES: u64 = JITTER_BUFFER_BYTES
    .saturating_add(RESAMPLER_STATE_BYTES)
    .saturating_add(DRIFT_STATE_BYTES);

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
