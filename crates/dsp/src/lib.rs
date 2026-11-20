//! Reducción de ruido opcional y bypass real para el audio de openrelay.
//!
//! El crate define el contrato [`NoiseSuppressor`] (procesar un bloque PCM16
//! mono en sitio y declarar su retardo) para que una etapa RNNoise o
//! DeepFilterNet pueda entrar más adelante sin tocar el consumidor. La cadena
//! [`NsChain`] tiene dos estados:
//!
//! - [`NsChain::bypass`]: **no hay etapa** ni cola. `process` devuelve `false`,
//!   deja el bloque intacto y sólo mueve contadores. Es una función `const`,
//!   así que construirla no asigna memoria.
//! - `NsChain::speexdsp` (feature `speexdsp`): una etapa `SpeexNs` sobre
//!   SpeexDSP 1.2.1 vendorizado. Declara un bloque de retardo; el número es un
//!   presupuesto declarado, no una medición.
//!
//! # Trabajador, no callback
//!
//! Tras construir la cadena, `process` no asigna memoria: sólo recorre el
//! bloque y llama al C. Está pensada para el worker del daemon, **nunca** para
//! el callback de tiempo real. La integración en `openrelay-daemon` es una tarea
//! posterior; este crate no la adelanta.
//!
//! # Feature
//!
//! `speexdsp` está apagada por defecto. Al activarla se compila el subconjunto
//! vendorizado de SpeexDSP con `cc` y se usa su NS clásico con la FFT
//! `smallft` (BSD-3-Clause). No se usa FFTW ni ninguna FFT GPL.
//!
//! # Ejemplo
//!
//! ```
//! use openrelay_dsp::NsChain;
//!
//! let mut chain = NsChain::bypass();
//! let mut block = [0i16; 240];
//! assert!(!chain.process(&mut block));
//! assert_eq!(chain.added_latency_samples(), 0);
//! assert_eq!(chain.counters().stages_run, 0);
//! ```

#![deny(unsafe_code)]
#![warn(missing_docs)]

#[allow(unsafe_code)]
#[cfg(feature = "speexdsp")]
pub mod speex;

/// Etapa de reducción de ruido intercambiable.
///
/// Una etapa procesa bloques PCM16 mono de tamaño fijo en sitio y declara
/// cuánto retardo algorítmico añade. El consumidor es [`NsChain`], que valida
/// la longitud del bloque antes de llamar.
pub trait NoiseSuppressor {
    /// Procesa un bloque PCM16 mono en sitio.
    ///
    /// El bloque debe medir exactamente la trama con la que se construyó la
    /// etapa. [`NsChain`] rechaza las longitudes distintas antes de llegar
    /// aquí; una llamada directa con otra longitud no debe escribir fuera del
    /// slice.
    fn process(&mut self, samples: &mut [i16]);

    /// Retardo algorítmico que la etapa añade, en muestras.
    fn added_latency_samples(&self) -> usize;
}

/// Contadores observables de una [`NsChain`].
///
/// Sólo cambian con las llamadas a [`NsChain::process`]; permiten demostrar
/// que el bypass no ejecutó ninguna etapa.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NsCounters {
    /// Bloques que pasaron por una etapa de DSP.
    pub stages_run: u64,
    /// Bloques presentados a `process`, incluidos los rechazados.
    pub frames_processed: u64,
    /// Bloques procesados sin etapa (bypass).
    pub frames_bypassed: u64,
    /// Bloques rechazados por no medir la trama configurada.
    pub frames_rejected: u64,
}

/// Error de configuración de una cadena o de una etapa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NsError {
    /// La frecuencia de muestreo o el tamaño de trama no son válidos.
    InvalidParams,
    /// SpeexDSP no pudo crear o configurar su estado.
    Init,
}

impl core::fmt::Display for NsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidParams => f.write_str("invalid frame or rate parameters"),
            Self::Init => f.write_str("SpeexDSP could not initialize the suppressor"),
        }
    }
}

impl std::error::Error for NsError {}

/// Cadena de reducción de ruido con bypass real.
///
/// Sin etapa no hay DSP ni cola: el bloque se devuelve intacto y sólo se
/// incrementan los contadores. El tamaño de trama se fija al construir la
/// cadena; un bloque de otra longitud se rechaza sin tocar la etapa.
pub struct NsChain {
    stage: Option<Box<dyn NoiseSuppressor>>,
    frame_samples: Option<usize>,
    latency_samples: usize,
    counters: NsCounters,
}

impl NsChain {
    /// Cadena de bypass: cero etapas, cero retardo y ninguna reserva.
    ///
    /// Es `const`, de modo que puede declararse como constante y el compilador
    /// demuestra que construirla no asigna memoria.
    pub const fn bypass() -> Self {
        Self {
            stage: None,
            frame_samples: None,
            latency_samples: 0,
            counters: NsCounters {
                stages_run: 0,
                frames_processed: 0,
                frames_bypassed: 0,
                frames_rejected: 0,
            },
        }
    }

    /// Cadena con una etapa intercambiable y una trama fija en muestras.
    ///
    /// El retardo declarado se toma de la propia etapa. Una trama de cero
    /// muestras no es un tamaño válido y devuelve [`NsError::InvalidParams`].
    pub fn with_stage(
        stage: Box<dyn NoiseSuppressor>,
        frame_samples: usize,
    ) -> Result<Self, NsError> {
        if frame_samples == 0 {
            return Err(NsError::InvalidParams);
        }
        let latency_samples = stage.added_latency_samples();
        Ok(Self {
            stage: Some(stage),
            frame_samples: Some(frame_samples),
            latency_samples,
            counters: NsCounters::default(),
        })
    }

    /// Cadena con el NS clásico de SpeexDSP para tramas de `frame_samples`.
    ///
    /// Pensada para 120, 240 o 480 muestras (2.5, 5 y 10 ms a 48 kHz), que son
    /// los perfiles de openrelay. Requiere la feature `speexdsp`.
    #[cfg(feature = "speexdsp")]
    pub fn speexdsp(sample_rate: u32, frame_samples: usize) -> Result<Self, NsError> {
        let stage = speex::SpeexNs::new(sample_rate, frame_samples)?;
        Self::with_stage(Box::new(stage), frame_samples)
    }

    /// Procesa un bloque en sitio y dice si corrió una etapa.
    ///
    /// Devuelve `false` en bypass o cuando el bloque no mide la trama
    /// configurada; en ese último caso el bloque queda intacto y el rechazo se
    /// cuenta en [`NsCounters::frames_rejected`].
    pub fn process(&mut self, samples: &mut [i16]) -> bool {
        self.counters.frames_processed += 1;
        if let Some(frame_samples) = self.frame_samples {
            if samples.len() != frame_samples {
                self.counters.frames_rejected += 1;
                return false;
            }
        }
        match self.stage.as_mut() {
            Some(stage) => {
                stage.process(samples);
                self.counters.stages_run += 1;
                true
            }
            None => {
                self.counters.frames_bypassed += 1;
                false
            }
        }
    }

    /// Copia de los contadores actuales.
    pub fn counters(&self) -> NsCounters {
        self.counters
    }

    /// Retardo declarado de la cadena, en muestras; cero en bypass.
    pub fn added_latency_samples(&self) -> usize {
        self.latency_samples
    }

    /// Trama configurada, o `None` si la cadena es bypass.
    pub fn frame_samples(&self) -> Option<usize> {
        self.frame_samples
    }

    /// `true` si no hay ninguna etapa de DSP.
    pub fn is_bypass(&self) -> bool {
        self.stage.is_none()
    }
}
