//! Control de drift por contadores de muestras y ocupación.
//!
//! El controlador estima el desfase de reloj con el cambio del contador
//! `entregadas - consumidas` por unidad de tiempo (ppm) y corrige la
//! ocupación del buffer con un término proporcional lento más un integrador
//! acotado. La salida es un ratio de resampling cercano a 1.0, limitado por
//! `max_correction_ppm` y por un slew máximo por segundo. El resync duro sólo
//! ocurre cuando la ocupación sale de la ventana configurada.
//!
//! No compara relojes de dispositivos: el instante es el reloj monotónico
//! local del receptor y sólo se usan diferencias.

use core::fmt;

use crate::pcm16::SAMPLE_RATE_HZ;

/// Ocupación objetivo por defecto, en frames de 5 ms (20 ms).
pub const DEFAULT_TARGET_FILL_FRAMES: u32 = 4;
/// Ventana por defecto alrededor del target, en frames (125 ms).
pub const DEFAULT_WINDOW_FRAMES: u32 = 25;
/// Corrección máxima por defecto, en ppm (±0.1 %).
pub const DEFAULT_MAX_CORRECTION_PPM: u32 = 1_000;
/// Slew máximo por defecto, en ppm por segundo.
pub const DEFAULT_SLEW_PPM_PER_S: u32 = 200;
/// Ganancia proporcional por defecto: ratio por frame de error.
pub const DEFAULT_KP_RATIO_PER_FRAME: f64 = 100e-6;
/// Ganancia integral por defecto: ratio por frame de error y segundo.
pub const DEFAULT_KI_RATIO_PER_FRAME_S: f64 = 10e-6;

/// Configuración del controlador; los campos son explícitos para que el
/// llamador fije la ventana y el slew, no sólo el target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftConfig {
    pub target_fill_frames: u32,
    pub window_frames: u32,
    pub max_correction_ppm: u32,
    pub slew_ppm_per_s: u32,
    pub kp_ratio_per_frame: f64,
    pub ki_ratio_per_frame_s: f64,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            target_fill_frames: DEFAULT_TARGET_FILL_FRAMES,
            window_frames: DEFAULT_WINDOW_FRAMES,
            max_correction_ppm: DEFAULT_MAX_CORRECTION_PPM,
            slew_ppm_per_s: DEFAULT_SLEW_PPM_PER_S,
            kp_ratio_per_frame: DEFAULT_KP_RATIO_PER_FRAME,
            ki_ratio_per_frame_s: DEFAULT_KI_RATIO_PER_FRAME_S,
        }
    }
}

impl DriftConfig {
    /// Valida la configuración; los valores no finitos se rechazan.
    pub fn validate(&self) -> Result<(), DriftError> {
        if !self.kp_ratio_per_frame.is_finite() || !self.ki_ratio_per_frame_s.is_finite() {
            return Err(DriftError::NonFiniteConfig);
        }
        if self.target_fill_frames == 0
            || self.window_frames == 0
            || self.max_correction_ppm == 0
            || self.max_correction_ppm > 10_000
            || self.slew_ppm_per_s == 0
            || self.kp_ratio_per_frame < 0.0
            || self.ki_ratio_per_frame_s < 0.0
        {
            return Err(DriftError::InvalidConfig("limits out of range"));
        }
        Ok(())
    }
}

/// Error tipado del controlador. Ninguna entrada inválida produce panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DriftError {
    /// Ganancia o constante no finita.
    NonFiniteConfig,
    /// Límite en cero, negativo o fuera de rango.
    InvalidConfig(&'static str),
    /// El reloj local no avanzó respecto de la observación previa.
    NonMonotonicTime,
    /// Los contadores de muestras retrocedieron.
    NonMonotonicCounters,
    /// Intervalo demasiado corto para estimar ppm.
    IntervalTooShort,
    /// Ocupación absurda (posible corrupción del llamador).
    FillOutOfRange,
}

impl fmt::Display for DriftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteConfig => write!(f, "non-finite drift configuration"),
            Self::InvalidConfig(what) => write!(f, "invalid drift configuration: {what}"),
            Self::NonMonotonicTime => write!(f, "local clock is not monotonic"),
            Self::NonMonotonicCounters => write!(f, "sample counters are not monotonic"),
            Self::IntervalTooShort => write!(f, "interval too short to estimate ppm"),
            Self::FillOutOfRange => write!(f, "occupancy outside a reasonable range"),
        }
    }
}

impl std::error::Error for DriftError {}

/// Resultado de una observación válida.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DriftUpdate {
    /// Ratio de resampling vigente (salida/entrada).
    pub ratio: f64,
    /// Deriva medida en el intervalo, en ppm (positiva = productor rápido).
    pub measured_ppm: f64,
    /// Ocupación observada del buffer, en frames.
    pub fill_frames: i64,
    /// Desvío respecto del target, en frames.
    pub error_frames: i64,
    /// `true` si esta observación disparó un resync por salir de ventana.
    pub hard_resync: bool,
}

/// Controlador de drift con estado acotado.
#[derive(Clone, Debug)]
pub struct DriftController {
    cfg: DriftConfig,
    ratio: f64,
    integral: f64,
    last_us: Option<u64>,
    first_us: Option<u64>,
    baseline_delivered: u64,
    last_delivered: u64,
    last_consumed: u64,
    measured_ppm: f64,
    resyncs: u64,
    updates: u64,
    peak_fill: i64,
    min_fill: i64,
}

impl DriftController {
    /// Crea el controlador validando la configuración.
    pub fn new(cfg: DriftConfig) -> Result<Self, DriftError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            ratio: 1.0,
            integral: 0.0,
            last_us: None,
            first_us: None,
            baseline_delivered: 0,
            last_delivered: 0,
            last_consumed: 0,
            measured_ppm: 0.0,
            resyncs: 0,
            updates: 0,
            peak_fill: 0,
            min_fill: 0,
        })
    }

    /// Ratio vigente; siempre finito y dentro de la corrección máxima.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Última deriva medida, en ppm.
    pub fn measured_ppm(&self) -> f64 {
        self.measured_ppm
    }

    /// Resyncs duros acumulados.
    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Observaciones válidas acumuladas.
    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Ocupación máxima observada, en frames.
    pub fn peak_fill_frames(&self) -> i64 {
        self.peak_fill
    }

    /// Ocupación mínima observada, en frames.
    pub fn min_fill_frames(&self) -> i64 {
        self.min_fill
    }

    /// Configuración efectiva.
    pub fn config(&self) -> DriftConfig {
        self.cfg
    }

    /// Observa ocupación y contadores de muestras del mismo reloj local.
    ///
    /// `delivered_samples` son las muestras entregadas por la red (las
    /// pérdidas no cuentan como deriva) y `consumed_samples` las que el
    /// resampler tomó como entrada. La diferencia da la deriva de reloj; la
    /// ocupación corrige el error lento.
    pub fn observe(
        &mut self,
        now_us: u64,
        fill_frames: i64,
        delivered_samples: u64,
        consumed_samples: u64,
    ) -> Result<DriftUpdate, DriftError> {
        if fill_frames.unsigned_abs() > 100_000 {
            return Err(DriftError::FillOutOfRange);
        }
        let target = i64::from(self.cfg.target_fill_frames);
        let error = fill_frames - target;
        let window = i64::from(self.cfg.window_frames);

        let Some(last_us) = self.last_us else {
            self.last_us = Some(now_us);
            self.first_us = Some(now_us);
            self.baseline_delivered = delivered_samples;
            self.last_delivered = delivered_samples;
            self.last_consumed = consumed_samples;
            self.measured_ppm = 0.0;
            self.updates = self.updates.saturating_add(1);
            self.peak_fill = fill_frames;
            self.min_fill = fill_frames;
            return Ok(DriftUpdate {
                ratio: self.ratio,
                measured_ppm: 0.0,
                fill_frames,
                error_frames: error,
                hard_resync: false,
            });
        };

        if now_us <= last_us {
            return Err(DriftError::NonMonotonicTime);
        }
        if delivered_samples < self.last_delivered || consumed_samples < self.last_consumed {
            return Err(DriftError::NonMonotonicCounters);
        }
        let dt_s = (now_us - last_us) as f64 / 1e6;
        if dt_s < 0.001 {
            return Err(DriftError::IntervalTooShort);
        }

        let first_us = self.first_us.unwrap_or(now_us);
        let elapsed_s = (now_us - first_us) as f64 / 1e6;
        let delivered_delta = delivered_samples.saturating_sub(self.baseline_delivered) as f64;
        let nominal = elapsed_s * f64::from(SAMPLE_RATE_HZ);
        let measured_ppm = if nominal > 0.0 {
            (delivered_delta - nominal) / nominal * 1_000_000.0
        } else {
            0.0
        };
        self.measured_ppm = measured_ppm;

        // Banda muerta de ±1 frame: el redondeo de ocupación (cola + anillo)
        // no debe enrollar el integrador durante una simulación larga.
        let control_error = if error.abs() <= 1 { 0 } else { error };

        let max_ratio = f64::from(self.cfg.max_correction_ppm) * 1e-6;
        let hard_resync = error.abs() > window;
        if hard_resync {
            self.integral = 0.0;
            self.ratio = (1.0 - measured_ppm * 1e-6).clamp(1.0 - max_ratio, 1.0 + max_ratio);
            self.resyncs = self.resyncs.saturating_add(1);
        } else {
            let ki_term = self.cfg.ki_ratio_per_frame_s * control_error as f64 * dt_s;
            // Integral acotado a una fracción de la corrección máxima para que
            // una saturación del buffer no lo deje enrollado.
            let integral_limit = max_ratio * 0.3;
            self.integral = (self.integral + ki_term).clamp(-integral_limit, integral_limit);
            // Más lleno que el target exige consumir más entrada por salida:
            // el ratio (salida/entrada) baja. El signo del error es negativo.
            let desired = (1.0
                - measured_ppm * 1e-6
                - self.cfg.kp_ratio_per_frame * control_error as f64
                - self.integral)
                .clamp(1.0 - max_ratio, 1.0 + max_ratio);
            let max_step = f64::from(self.cfg.slew_ppm_per_s) * 1e-6 * dt_s;
            let delta = (desired - self.ratio).clamp(-max_step, max_step);
            self.ratio = (self.ratio + delta).clamp(1.0 - max_ratio, 1.0 + max_ratio);
        }

        self.last_us = Some(now_us);
        self.last_delivered = delivered_samples;
        self.last_consumed = consumed_samples;
        self.peak_fill = self.peak_fill.max(fill_frames);
        self.min_fill = self.min_fill.min(fill_frames);
        self.updates = self.updates.saturating_add(1);

        Ok(DriftUpdate {
            ratio: self.ratio,
            measured_ppm,
            fill_frames,
            error_frames: error,
            hard_resync,
        })
    }
}
