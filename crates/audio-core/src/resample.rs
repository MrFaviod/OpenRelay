//! Remuestreador lineal de streaming con ratio externo.
//!
//! El ratio es **entrada real** del algoritmo: cada llamada a `pull_frame`
//! consume `SAMPLE_COUNT / ratio` muestras de entrada y produce exactamente
//! `SAMPLE_COUNT` muestras de salida con interpolación lineal. El anillo de
//! entrada es un array fijo; no asigna memoria y no usa `unsafe`.
//!
//! El controlador de drift ajusta el ratio con `set_ratio`; este módulo no
//! decide la corrección por su cuenta.

use core::fmt;

use crate::pcm16::SAMPLE_COUNT;

/// Capacidad del anillo de entrada: dos frames y un margen mínimo.
pub const RING_LEN: usize = 2 * SAMPLE_COUNT + 8;
/// Ratio mínimo aceptado (se clampa, no se rechaza).
pub const RATIO_MIN: f64 = 0.98;
/// Ratio máximo aceptado (se clampa, no se rechaza).
pub const RATIO_MAX: f64 = 1.02;

/// Error tipado del resampler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResampleError {
    /// Ratio `NaN` o infinito.
    NonFiniteRatio,
    /// Ratio no positivo o absurdo (se rechaza antes de clamp).
    RatioOutOfRange,
    /// El frame de entrada no cabe en el anillo fijo.
    RingFull,
}

impl fmt::Display for ResampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteRatio => write!(f, "ratio de resampling no finito"),
            Self::RatioOutOfRange => write!(f, "ratio de resampling fuera de rango"),
            Self::RingFull => write!(f, "anillo de entrada del resampler lleno"),
        }
    }
}

impl std::error::Error for ResampleError {}

/// Remuestreador lineal con estado de fase entre frames.
#[derive(Clone, Debug)]
pub struct LinearResampler {
    ring: [i16; RING_LEN],
    head: usize,
    len: usize,
    ratio: f64,
    frac: f64,
    current: i16,
    primed: bool,
    consumed: u64,
    produced: u64,
    underruns: u64,
    overflows: u64,
}

impl LinearResampler {
    /// Crea el resampler con un ratio inicial validado.
    pub fn new(ratio: f64) -> Result<Self, ResampleError> {
        let ratio = validate_ratio(ratio)?;
        Ok(Self {
            ring: [0; RING_LEN],
            head: 0,
            len: 0,
            ratio,
            frac: 0.0,
            current: 0,
            primed: false,
            consumed: 0,
            produced: 0,
            underruns: 0,
            overflows: 0,
        })
    }

    /// Ratio efectivo (salida/entrada).
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Cambia el ratio; rechaza no finitos y clampa a `[RATIO_MIN, RATIO_MAX]`.
    pub fn set_ratio(&mut self, ratio: f64) -> Result<f64, ResampleError> {
        self.ratio = validate_ratio(ratio)?;
        Ok(self.ratio)
    }

    /// Muestras de entrada consumidas desde el inicio.
    pub fn consumed_input(&self) -> u64 {
        self.consumed
    }

    /// Muestras de salida producidas desde el inicio.
    pub fn produced_output(&self) -> u64 {
        self.produced
    }

    /// Underruns (salidas con el anillo vacío; se mantiene la última muestra).
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Frames rechazados por no caber en el anillo.
    pub fn overflows(&self) -> u64 {
        self.overflows
    }

    /// Muestras actualmente en el anillo.
    pub fn buffered_samples(&self) -> usize {
        self.len
    }

    /// Encola un frame completo sin asignar memoria.
    pub fn push_frame(&mut self, frame: &[i16; SAMPLE_COUNT]) -> Result<(), ResampleError> {
        if self.len + SAMPLE_COUNT > RING_LEN {
            self.overflows = self.overflows.saturating_add(1);
            return Err(ResampleError::RingFull);
        }
        for (i, sample) in frame.iter().enumerate() {
            self.ring[(self.head + self.len + i) % RING_LEN] = *sample;
        }
        self.len += SAMPLE_COUNT;
        Ok(())
    }

    /// Produce un frame de salida consumiendo el ratio vigente.
    ///
    /// Devuelve cuántas muestras escribió (`SAMPLE_COUNT` si hay al menos una
    /// muestra de entrada o estado previo; 0 sólo antes del primer push).
    pub fn pull_frame(&mut self, out: &mut [i16; SAMPLE_COUNT]) -> usize {
        if !self.primed {
            if self.len == 0 {
                self.underruns = self.underruns.saturating_add(1);
                out.fill(0);
                return 0;
            }
            self.current = self.pop_sample();
            self.primed = true;
        }

        let step = 1.0 / self.ratio;
        for slot in out.iter_mut() {
            let next = if self.len > 0 {
                self.ring[self.head]
            } else {
                self.current
            };
            let interpolated =
                f64::from(self.current) + (f64::from(next) - f64::from(self.current)) * self.frac;
            *slot = clamp_i16(interpolated);
            self.produced = self.produced.saturating_add(1);

            self.frac += step;
            while self.frac >= 1.0 {
                if self.len == 0 {
                    break; // se mantiene la última muestra hasta el próximo frame
                }
                self.current = self.pop_sample();
                self.frac -= 1.0;
            }
        }
        SAMPLE_COUNT
    }

    fn pop_sample(&mut self) -> i16 {
        let sample = self.ring[self.head];
        self.head = (self.head + 1) % RING_LEN;
        self.len -= 1;
        self.consumed = self.consumed.saturating_add(1);
        sample
    }
}

fn validate_ratio(ratio: f64) -> Result<f64, ResampleError> {
    if !ratio.is_finite() {
        return Err(ResampleError::NonFiniteRatio);
    }
    if ratio <= 0.0 || ratio > 10.0 {
        return Err(ResampleError::RatioOutOfRange);
    }
    Ok(ratio.clamp(RATIO_MIN, RATIO_MAX))
}

fn clamp_i16(value: f64) -> i16 {
    let rounded = value.round();
    if rounded >= f64::from(i16::MAX) {
        i16::MAX
    } else if rounded <= f64::from(i16::MIN) {
        i16::MIN
    } else {
        rounded as i16
    }
}
