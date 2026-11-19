//! Envoltorio seguro de SpeexDSP 1.2.1 para la feature `speexdsp`.
//!
//! Usa el NS clásico: `speex_preprocess_state_init`, el control
//! `SPEEX_PREPROCESS_SET_DENOISE` y `speex_preprocess_run`. La FFT es
//! `smallft`, integrada en el vendor y con licencia BSD-3-Clause; no se activa
//! FFTW.
//!
//! Este es el **único** módulo del crate con `unsafe`: la API C no tiene un
//! crate seguro que la envuelva (los `speexdsp-sys` de crates.io exigen una
//! `libspeexdsp` del sistema que aquí no existe). La excepción está declarada
//! con `#[allow(unsafe_code)]` sobre el módulo en `lib.rs` y cada bloque lleva
//! su comentario `SAFETY`.
//!
//! # Retardo
//!
//! El supresor declara un bloque de retardo algorítmico. Es el presupuesto que
//! fija el plan para este perfil; no es una medición end-to-end.

use std::ffi::{c_int, c_void};

use crate::{NoiseSuppressor, NsError};

/// Control de SpeexDSP que activa o desactiva el supresor.
const SPEEX_PREPROCESS_SET_DENOISE: c_int = 0;

/// Trama mínima aceptada, en muestras.
const MIN_FRAME_SAMPLES: usize = 8;
/// Trama máxima aceptada, en muestras (la FFT del preprocesador llega a 2048).
const MAX_FRAME_SAMPLES: usize = 1024;

/// Estado opaco de SpeexDSP. Sólo se maneja mediante puntero.
#[repr(C)]
struct SpeexPreprocessState {
    _opaque: [u8; 0],
}

extern "C" {
    fn speex_preprocess_state_init(
        frame_size: c_int,
        sampling_rate: c_int,
    ) -> *mut SpeexPreprocessState;
    fn speex_preprocess_state_destroy(st: *mut SpeexPreprocessState);
    fn speex_preprocess_ctl(
        st: *mut SpeexPreprocessState,
        request: c_int,
        ptr: *mut c_void,
    ) -> c_int;
    fn speex_preprocess_run(st: *mut SpeexPreprocessState, x: *mut i16) -> c_int;
}

/// Supresor de ruido de SpeexDSP para una trama fija.
///
/// El estado C es propiedad exclusiva del objeto: se crea en [`SpeexNs::new`]
/// y se libera en `Drop`. `process` no asigna memoria ni cambia el tamaño de
/// trama.
pub struct SpeexNs {
    state: *mut SpeexPreprocessState,
    frame_samples: usize,
    latency_samples: usize,
}

impl SpeexNs {
    /// Crea el supresor para `frame_samples` muestras a `sample_rate` Hz.
    ///
    /// Activa el denoise y declara un bloque de retardo. Rechaza frecuencias o
    /// tramas fuera de rango antes de llamar al C.
    pub fn new(sample_rate: u32, frame_samples: usize) -> Result<Self, NsError> {
        if sample_rate == 0 || !(MIN_FRAME_SAMPLES..=MAX_FRAME_SAMPLES).contains(&frame_samples) {
            return Err(NsError::InvalidParams);
        }

        // SAFETY: `frame_samples` y `sample_rate` están validados y caben en
        // `c_int`; init no retiene punteros a Rust y devuelve un estado propio
        // o NULL.
        let state =
            unsafe { speex_preprocess_state_init(frame_samples as c_int, sample_rate as c_int) };
        if state.is_null() {
            return Err(NsError::Init);
        }

        let mut enabled: c_int = 1;
        // SAFETY: `state` es un puntero no nulo creado arriba y de propiedad
        // exclusiva; `SET_DENOISE` lee un `c_int` del puntero y no lo conserva.
        let rc = unsafe {
            speex_preprocess_ctl(
                state,
                SPEEX_PREPROCESS_SET_DENOISE,
                (&mut enabled as *mut c_int).cast::<c_void>(),
            )
        };
        if rc != 0 {
            // SAFETY: el estado sigue siendo propiedad de este constructor y
            // no se ha compartido ni liberado.
            unsafe { speex_preprocess_state_destroy(state) };
            return Err(NsError::Init);
        }

        Ok(Self {
            state,
            frame_samples,
            latency_samples: frame_samples,
        })
    }
}

impl NoiseSuppressor for SpeexNs {
    fn process(&mut self, samples: &mut [i16]) {
        if samples.len() != self.frame_samples {
            return;
        }
        // SAFETY: `state` no es nulo y es propiedad exclusiva de `self`;
        // `samples` mide exactamente `frame_size` i16, que es lo único que
        // `speex_preprocess_run` lee y escribe.
        unsafe { speex_preprocess_run(self.state, samples.as_mut_ptr()) };
    }

    fn added_latency_samples(&self) -> usize {
        self.latency_samples
    }
}

impl Drop for SpeexNs {
    fn drop(&mut self) {
        // SAFETY: `state` procede de `speex_preprocess_state_init`, este tipo
        // es su único propietario y no se liberó antes.
        unsafe { speex_preprocess_state_destroy(self.state) };
    }
}
