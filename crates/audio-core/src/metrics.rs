//! Contadores y métricas del core de audio.
//!
//! Todo incremento satura en `u64::MAX`: un flujo hostil no puede desbordar un
//! contador y provocar un panic en release/debug. No hay IO ni logging aquí.

/// Incrementa `counter` con saturación.
pub fn inc(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

/// Suma `value` con saturación.
pub fn add(counter: &mut u64, value: u64) {
    *counter = counter.saturating_add(value);
}

/// Contadores del jitter buffer. `emitted` cuenta sólo frames de red; PLC y
/// silencio se contabilizan aparte para no confundir audio real con relleno.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterCounters {
    /// Frames aceptados en la cola (incluye reordenados y cambios de época).
    pub accepted: u64,
    /// Frames de red entregados al consumidor.
    pub emitted: u64,
    /// Frames que llegaron después de que su deadline ya pasó.
    pub dropped_late: u64,
    /// Copias de una secuencia todavía presente en la cola.
    pub dropped_duplicate: u64,
    /// Frames de una época anterior; nunca resetean el flujo vigente.
    pub dropped_old_epoch: u64,
    /// Frames fuera de la ventana de capacidad o colisión de slot.
    pub dropped_overflow: u64,
    /// Frames rechazados por estado interno inconsistente.
    pub dropped_invalid: u64,
    /// Frames aceptados con secuencia menor que la última vista.
    pub reordered: u64,
    /// Periodos de 5 ms sin frame de red (huecos).
    pub gaps: u64,
    /// Huecos rellenados con PLC a partir del último frame.
    pub plc_frames: u64,
    /// Huecos rellenados con silencio tras agotar el PLC.
    pub silence_frames: u64,
    /// Resyncs por época nueva, discontinuity con salto o consumidor atrasado.
    pub resets: u64,
    /// Lecturas de reloj local que retrocedieron; se clamped, no se comparan.
    pub clock_regressions: u64,
}

impl JitterCounters {
    pub fn record_accepted(&mut self) {
        inc(&mut self.accepted);
    }

    pub fn record_emitted(&mut self) {
        inc(&mut self.emitted);
    }

    pub fn record_dropped_late(&mut self) {
        inc(&mut self.dropped_late);
    }

    pub fn record_dropped_duplicate(&mut self) {
        inc(&mut self.dropped_duplicate);
    }

    pub fn record_dropped_old_epoch(&mut self) {
        inc(&mut self.dropped_old_epoch);
    }

    pub fn record_dropped_overflow(&mut self) {
        inc(&mut self.dropped_overflow);
    }

    pub fn record_dropped_invalid(&mut self) {
        inc(&mut self.dropped_invalid);
    }

    pub fn record_reordered(&mut self) {
        inc(&mut self.reordered);
    }

    pub fn record_gap(&mut self) {
        inc(&mut self.gaps);
    }

    pub fn record_plc(&mut self) {
        inc(&mut self.plc_frames);
    }

    pub fn record_silence(&mut self) {
        inc(&mut self.silence_frames);
    }

    pub fn record_reset(&mut self) {
        inc(&mut self.resets);
    }

    pub fn record_clock_regression(&mut self) {
        inc(&mut self.clock_regressions);
    }

    /// Frames descartados por cualquier causa, con saturación.
    pub fn dropped_total(&self) -> u64 {
        self.dropped_late
            .saturating_add(self.dropped_duplicate)
            .saturating_add(self.dropped_old_epoch)
            .saturating_add(self.dropped_overflow)
            .saturating_add(self.dropped_invalid)
    }

    /// Salidas totales del buffer: red + PLC + silencio.
    pub fn emitted_total(&self) -> u64 {
        self.emitted
            .saturating_add(self.plc_frames)
            .saturating_add(self.silence_frames)
    }
}

/// Jitter de llegada medido contra el periodo nominal de 5 ms, siempre en el
/// reloj local del receptor. `sum` y `max` saturan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterMetrics {
    pub arrival_jitter_us_max: u64,
    pub arrival_jitter_us_sum: u64,
    pub arrival_jitter_samples: u64,
}

impl JitterMetrics {
    /// Registra una desviación absoluta respecto del periodo nominal.
    pub fn record(&mut self, deviation_us: u64) {
        if deviation_us > self.arrival_jitter_us_max {
            self.arrival_jitter_us_max = deviation_us;
        }
        add(&mut self.arrival_jitter_us_sum, deviation_us);
        inc(&mut self.arrival_jitter_samples);
    }

    /// Media entera de las desviaciones registradas; 0 sin muestras.
    pub fn mean_us(&self) -> u64 {
        if self.arrival_jitter_samples == 0 {
            0
        } else {
            self.arrival_jitter_us_sum / self.arrival_jitter_samples
        }
    }
}
