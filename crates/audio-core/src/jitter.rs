//! Jitter buffer acotado con entrega por deadline, huecos/PLC y contadores.
//!
//! ## Reloj
//!
//! El buffer **no compara relojes de dispositivos distintos**. Productor y
//! consumidor entregan timestamps de un reloj monotónico local en
//! microsegundos; sólo se usan diferencias dentro de ese mismo reloj. Si el
//! reloj local retrocede, el valor se clampa y se cuenta `clock_regressions`.
//!
//! ## Memoria y camino de consumo
//!
//! La cola es un array fijo de `CAPACITY_FRAMES` slots (`AudioFrame` completos),
//! sin `Vec`, sin locks, sin IO y sin `unsafe`. `push`/`pop` no asignan memoria:
//! devuelven frames por valor y descartan lo que no cabe. Bajo flood la cola se
//! queda en la capacidad y el exceso se contabiliza.
//!
//! ## Deadline
//!
//! El deadline vive en una rejilla fija anclada a la primera llegada más el
//! target configurado; cada emisión suma exactamente 5 ms. Las llegadas nuevas
//! no lo mueven, así que una ráfaga no puede posponer indefinidamente la
//! entrega.

use crate::frame::{AudioFrame, Flags, DURATION_US};
use crate::metrics::{JitterCounters, JitterMetrics};
use crate::pcm16::{Samples, SILENCE};

/// Target mínimo del buffer, en milisegundos (contrato de configuración).
pub const TARGET_MIN_MS: u16 = 5;
/// Target máximo del buffer, en milisegundos (techo explícito).
pub const TARGET_MAX_MS: u16 = 40;
/// Target por defecto del buffer, en milisegundos.
pub const TARGET_DEFAULT_MS: u16 = 10;
/// Periodo nominal de un frame del perfil v1, en microsegundos.
pub const FRAME_US: u64 = DURATION_US as u64;
/// Slots fijos de la cola: 16 frames = 80 ms de audio.
pub const CAPACITY_FRAMES: usize = 16;
/// Ventana temporal de la cola, en microsegundos.
pub const CAPACITY_US: u64 = CAPACITY_FRAMES as u64 * FRAME_US;
/// Huecos consecutivos que se rellenan con PLC antes de pasar a silencio.
const PLC_FADE_FRAMES: u8 = 3;

/// Configuración del jitter buffer. El target se limita a `5..=40` ms; un
/// valor fuera de rango no paniquea, se clampa.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitterConfig {
    target_ms: u16,
}

impl JitterConfig {
    /// Crea la configuración clampeando el target a la ventana del contrato.
    pub const fn new(target_ms: u16) -> Self {
        let target_ms = if target_ms < TARGET_MIN_MS {
            TARGET_MIN_MS
        } else if target_ms > TARGET_MAX_MS {
            TARGET_MAX_MS
        } else {
            target_ms
        };
        Self { target_ms }
    }

    /// Target efectivo tras el clamp, en milisegundos.
    pub const fn target_ms(self) -> u16 {
        self.target_ms
    }

    /// Target efectivo en microsegundos.
    pub const fn target_us(self) -> u64 {
        self.target_ms as u64 * 1_000
    }
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self::new(TARGET_DEFAULT_MS)
    }
}

/// Tipo de audio entregado por `pop`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmitKind {
    /// Frame real recibido de la red.
    Network,
    /// Frame sintetizado a partir del último frame (PLC con caída).
    Plc,
    /// Silencio explícito tras agotar el PLC.
    Silence,
}

/// Salida de una emisión: frame de tamaño fijo y su procedencia.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitterEmit {
    pub frame: AudioFrame,
    pub kind: EmitKind,
}

/// Resultado de `pop`.
///
/// El variant grande es intencional: el frame es de tamaño fijo y el camino de
/// consumo no puede reservar memoria ni bloquear, así que no se boxea.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PopOutcome {
    /// El deadline del próximo frame todavía no llegó.
    NotDue,
    /// Hay un frame listo para el consumidor.
    Emitted(JitterEmit),
}

/// Resultado de `push`; permite afirmar en tests y contar la causa.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// Frame nuevo, en orden, dentro de la ventana.
    Accepted,
    /// Frame nuevo con secuencia menor que la última vista (reorden).
    Reordered,
    /// Primer frame de una época más nueva; resetea la cola.
    NewEpoch,
    /// `discontinuity` con salto mayor que la capacidad; resetea la cola.
    Resynced,
    /// Su deadline ya pasó (secuencia anterior al playhead).
    Late,
    /// Copia de una secuencia todavía encolada.
    Duplicate,
    /// Época anterior; se descarta sin tocar el estado vigente.
    OldEpoch,
    /// Fuera de la ventana de capacidad o colisión de slot.
    Overflow,
}

/// Jitter buffer de tamaño fijo.
pub struct JitterBuffer {
    cfg: JitterConfig,
    slots: [Option<(u64, AudioFrame)>; CAPACITY_FRAMES],
    occupancy: usize,
    peak_occupancy: usize,
    epoch: u32,
    next_seq: u64,
    newest_seq: u64,
    deadline_us: u64,
    last_now_us: u64,
    last_arrival_us: u64,
    has_arrival: bool,
    last_frame: Samples,
    plc_run: u8,
    primed: bool,
    started: bool,
    counters: JitterCounters,
    metrics: JitterMetrics,
}

impl JitterBuffer {
    /// Crea un buffer vacío con capacidad fija; no asigna memoria dinámica.
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            cfg,
            slots: [const { None }; CAPACITY_FRAMES],
            occupancy: 0,
            peak_occupancy: 0,
            epoch: 1,
            next_seq: 0,
            newest_seq: 0,
            deadline_us: 0,
            last_now_us: 0,
            last_arrival_us: 0,
            has_arrival: false,
            last_frame: SILENCE,
            plc_run: 0,
            primed: false,
            started: false,
            counters: JitterCounters::default(),
            metrics: JitterMetrics::default(),
        }
    }

    /// Target efectivo en milisegundos.
    pub fn target_ms(&self) -> u16 {
        self.cfg.target_ms()
    }

    /// Deadline actual del próximo frame, en el reloj local.
    pub fn deadline_us(&self) -> u64 {
        self.deadline_us
    }

    /// Slots ocupados ahora mismo (siempre `<= CAPACITY_FRAMES`).
    pub fn occupancy(&self) -> usize {
        self.occupancy
    }

    /// Máximo de slots ocupados observado (cota de memoria viva).
    pub fn peak_occupancy(&self) -> usize {
        self.peak_occupancy
    }

    /// Contadores acumulados.
    pub fn counters(&self) -> JitterCounters {
        self.counters
    }

    /// Métricas de jitter de llegada del reloj local.
    pub fn metrics(&self) -> JitterMetrics {
        self.metrics
    }

    /// Encola un frame con su instante de llegada local. Nunca paniquea.
    pub fn push(&mut self, frame: AudioFrame, arrival_us: u64) -> PushOutcome {
        let arrival = self.clamp_local(arrival_us);

        if !self.primed {
            self.primed = true;
            self.epoch = frame.epoch();
            let abs = u64::from(frame.seq());
            self.next_seq = abs;
            self.newest_seq = abs;
            self.deadline_us = arrival.saturating_add(self.cfg.target_us());
            self.insert(frame, abs);
            self.counters.record_accepted();
            self.record_arrival(arrival);
            return PushOutcome::Accepted;
        }

        if frame.epoch() != self.epoch {
            let diff = frame.epoch().wrapping_sub(self.epoch) as i32;
            if diff > 0 && diff != i32::MIN {
                let abs = u64::from(frame.seq());
                self.reset_to(frame.epoch(), abs, arrival);
                self.insert(frame, abs);
                self.counters.record_accepted();
                self.record_arrival(arrival);
                return PushOutcome::NewEpoch;
            }
            self.counters.record_dropped_old_epoch();
            return PushOutcome::OldEpoch;
        }

        let delta = frame.seq().wrapping_sub(self.newest_seq as u32) as i32;
        let abs_i = self.newest_seq as i64 + i64::from(delta);
        if abs_i < 0 {
            self.counters.record_dropped_late();
            return PushOutcome::Late;
        }
        let abs = abs_i as u64;
        if abs < self.next_seq {
            self.counters.record_dropped_late();
            return PushOutcome::Late;
        }
        if frame.flags().is_discontinuity()
            && abs >= self.next_seq.saturating_add(CAPACITY_FRAMES as u64)
        {
            self.reset_to(self.epoch, abs, arrival);
            self.insert(frame, abs);
            self.counters.record_accepted();
            self.record_arrival(arrival);
            return PushOutcome::Resynced;
        }
        if abs >= self.next_seq.saturating_add(CAPACITY_FRAMES as u64) {
            self.counters.record_dropped_overflow();
            return PushOutcome::Overflow;
        }

        let idx = (abs % CAPACITY_FRAMES as u64) as usize;
        if let Some((existing, _)) = &self.slots[idx] {
            if *existing == abs {
                self.counters.record_dropped_duplicate();
                return PushOutcome::Duplicate;
            }
            // Imposible con la ventana validada; se trata como inconsistencia.
            self.counters.record_dropped_invalid();
            return PushOutcome::Overflow;
        }

        let reordered = abs < self.newest_seq;
        if !reordered {
            self.newest_seq = abs;
        }
        self.insert(frame, abs);
        self.counters.record_accepted();
        self.record_arrival(arrival);
        if reordered {
            self.counters.record_reordered();
            PushOutcome::Reordered
        } else {
            PushOutcome::Accepted
        }
    }

    /// Entrega el frame cuyo deadline venció, o `NotDue`.
    ///
    /// Si el consumidor se atrasó más que la ventana completa, se resincroniza
    /// (descarta lo pendiente y reancla el deadline), en vez de arrastrar una
    /// deuda de silencio ilimitada.
    pub fn pop(&mut self, now_us: u64) -> PopOutcome {
        let now = self.clamp_local(now_us);
        if !self.primed {
            return PopOutcome::NotDue;
        }
        if self.started && now > self.deadline_us.saturating_add(CAPACITY_US) {
            self.flush_pending();
            self.next_seq = self.newest_seq.saturating_add(1);
            self.deadline_us = now.saturating_add(self.cfg.target_us());
            self.started = false;
            self.plc_run = 0;
            self.counters.record_reset();
            return PopOutcome::NotDue;
        }
        if now < self.deadline_us {
            return PopOutcome::NotDue;
        }

        self.started = true;
        self.deadline_us = self.deadline_us.saturating_add(FRAME_US);
        let seq = self.next_seq;

        let idx = (seq % CAPACITY_FRAMES as u64) as usize;
        let matches = matches!(&self.slots[idx], Some((abs, _)) if *abs == seq);
        if matches {
            if let Some((_, frame)) = self.slots[idx].take() {
                self.occupancy = self.occupancy.saturating_sub(1);
                self.last_frame = *frame.samples();
                self.plc_run = 0;
                self.counters.record_emitted();
                self.next_seq = self.next_seq.saturating_add(1);
                return PopOutcome::Emitted(JitterEmit {
                    frame,
                    kind: EmitKind::Network,
                });
            }
        }

        // Hueco: PLC con caída y, agotado, silencio explícito.
        self.counters.record_gap();
        self.plc_run = self.plc_run.saturating_add(1);
        self.next_seq = self.next_seq.saturating_add(1);

        let (samples, flags, kind) = if self.plc_run <= PLC_FADE_FRAMES {
            let gain = 1i32 << self.plc_run;
            let mut faded = self.last_frame;
            for sample in faded.iter_mut() {
                *sample = (*sample as i32 / gain) as i16;
            }
            self.counters.record_plc();
            (faded, Flags::DISCONTINUITY, EmitKind::Plc)
        } else {
            self.counters.record_silence();
            (
                SILENCE,
                Flags::SILENCE | Flags::DISCONTINUITY,
                EmitKind::Silence,
            )
        };

        match self.make_frame(seq, flags, samples) {
            Some(frame) => PopOutcome::Emitted(JitterEmit { frame, kind }),
            None => PopOutcome::NotDue,
        }
    }

    fn make_frame(&self, seq: u64, flags: Flags, samples: Samples) -> Option<AudioFrame> {
        AudioFrame::new(self.epoch, seq as u32, flags, samples).ok()
    }

    fn insert(&mut self, frame: AudioFrame, abs: u64) {
        let idx = (abs % CAPACITY_FRAMES as u64) as usize;
        self.slots[idx] = Some((abs, frame));
        self.occupancy = self.occupancy.saturating_add(1);
        if self.occupancy > self.peak_occupancy {
            self.peak_occupancy = self.occupancy;
        }
    }

    fn reset_to(&mut self, epoch: u32, abs: u64, arrival_us: u64) {
        self.flush_pending();
        self.epoch = epoch;
        self.next_seq = abs;
        self.newest_seq = abs;
        self.deadline_us = arrival_us.saturating_add(self.cfg.target_us());
        self.started = false;
        self.plc_run = 0;
        self.last_frame = SILENCE;
        self.counters.record_reset();
    }

    fn flush_pending(&mut self) {
        for slot in self.slots.iter_mut() {
            *slot = None;
        }
        self.occupancy = 0;
    }

    fn record_arrival(&mut self, arrival_us: u64) {
        if self.has_arrival {
            let delta = arrival_us.saturating_sub(self.last_arrival_us);
            self.metrics.record(delta.abs_diff(FRAME_US));
        }
        self.last_arrival_us = arrival_us;
        self.has_arrival = true;
    }

    fn clamp_local(&mut self, now_us: u64) -> u64 {
        if now_us < self.last_now_us {
            self.counters.record_clock_regression();
            self.last_now_us
        } else {
            self.last_now_us = now_us;
            now_us
        }
    }
}
