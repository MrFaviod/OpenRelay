//! SPSC de capacidad fija para el camino de captura de openrelay.
//!
//! El anillo transporta **muestras** `i16` del perfil PCM16 mono 48 kHz de
//! `openrelay-audio-core`, no frames de 240 muestras. Esa decisión es deliberada: los
//! buffers de PipeWire llegan con tamaños arbitrarios (y pueden cambiar entre
//! callbacks), de modo que cualquier unidad mayor que la muestra obligaría a
//! partir o recomponer frames en el callback RT. Una FIFO de muestras conserva
//! el orden y no pierde nada al cruzar un callback, y cada consumidor decide
//! dónde empieza su frame.
//!
//! Política de desbordamiento: se escribe el prefijo que cabe y se descarta el
//! resto. Nunca se bloquea, nunca se redimensiona. El descarte se cuenta en el
//! productor; el subconsumo se cuenta en el consumidor y se rellena con
//! silencio.

use openrelay_audio_core::BYTES_PER_SAMPLE;
use rtrb::{Consumer, Producer, RingBuffer};

/// Resultado de una escritura en el anillo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushOutcome {
    /// Muestras aceptadas.
    pub written: usize,
    /// Muestras descartadas por falta de capacidad.
    pub dropped: usize,
}

/// Resultado de un render PCM16LE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderOutcome {
    /// Frames escritos en el destino (incluye los rellenados con silencio).
    pub frames: usize,
    /// Frames que no había en el anillo y se rellenaron con silencio.
    pub underrun_frames: usize,
}

/// Productor único del anillo. Vive en el hilo propietario (fuera de RT).
pub struct SampleProducer {
    inner: Producer<i16>,
    capacity: usize,
    dropped: u64,
}

/// Consumidor único del anillo. Vive en el callback de proceso de PipeWire.
pub struct SampleConsumer {
    inner: Consumer<i16>,
    capacity: usize,
}

/// Crea un par SPSC de capacidad fija en muestras.
///
/// La capacidad se reserva aquí, en el hilo llamante: el callback RT sólo toca
/// memoria ya inicializada. `capacity == 0` es válido y descarta todo.
#[must_use]
pub fn ring(capacity: usize) -> (SampleProducer, SampleConsumer) {
    let (producer, consumer) = RingBuffer::<i16>::new(capacity);
    (
        SampleProducer {
            inner: producer,
            capacity,
            dropped: 0,
        },
        SampleConsumer {
            inner: consumer,
            capacity,
        },
    )
}

impl SampleProducer {
    /// Capacidad fija en muestras.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Muestras que aún caben.
    pub fn free(&self) -> usize {
        self.inner.slots()
    }

    /// Total acumulado de muestras descartadas por desbordamiento.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Escribe el prefijo que cabe y descarta el resto.
    ///
    /// Sin bloqueo, sin crecimiento, sin asignaciones.
    pub fn push_slice(&mut self, samples: &[i16]) -> PushOutcome {
        let free_before = self.inner.slots();
        let n = samples.len().min(free_before);
        let mut pushed = 0usize;
        for &sample in &samples[..n] {
            // `slots()` garantiza capacidad; si aun así fallara, se corta y lo
            // no escrito se contabiliza como descarte, nunca pánico en RT.
            if self.inner.push(sample).is_err() {
                break;
            }
            pushed += 1;
        }
        let dropped = samples.len() - pushed;
        self.dropped += dropped as u64;
        PushOutcome {
            written: pushed,
            dropped,
        }
    }
}

impl SampleConsumer {
    /// Capacidad fija en muestras.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Muestras disponibles.
    pub fn available(&self) -> usize {
        self.inner.slots()
    }

    /// Renderiza hasta `frames` frames PCM16LE en `dst`.
    ///
    /// Esta es **la** función que llama el callback de proceso de PipeWire: los
    /// tests la ejercitan tal cual, no una copia. Escribe en
    /// `dst[offset + i * stride ..]` con `i16::to_le_bytes`, el mismo wire que
    /// `openrelay_audio_core::pcm16` (`BYTES_PER_SAMPLE` bytes por muestra, LE).
    ///
    /// - `stride` menor que el tamaño de muestra o no alineado se trata como
    ///   configuración inválida: no se escribe nada y no se cuenta underrun.
    /// - Frames que faltan se rellenan con silencio; los bytes de relleno dentro
    ///   de un slot con `stride > 2` se ponen a cero para no dejar residuo del
    ///   buffer anterior.
    /// - El destino se recorta por longitud: nunca se escribe fuera de `dst`.
    pub fn render_pcm16le_into(
        &mut self,
        dst: &mut [u8],
        offset: usize,
        stride: usize,
        frames: usize,
    ) -> RenderOutcome {
        if stride < BYTES_PER_SAMPLE
            || offset >= dst.len()
            || frames == 0
            || stride % BYTES_PER_SAMPLE != 0
        {
            return RenderOutcome::default();
        }

        let capacity_frames = (dst.len() - offset) / stride;
        let want = frames.min(capacity_frames);
        let mut underrun = 0usize;

        for i in 0..want {
            let start = offset + i * stride;
            let end = start + stride;
            let slot = &mut dst[start..end];
            let sample = match self.inner.pop() {
                Ok(sample) => sample,
                Err(_) => {
                    underrun += 1;
                    0
                }
            };
            // Silencio explícito: evita residuo de un callback anterior.
            slot.fill(0);
            slot[..BYTES_PER_SAMPLE].copy_from_slice(&sample.to_le_bytes());
        }

        RenderOutcome {
            frames: want,
            underrun_frames: underrun,
        }
    }
}

impl std::fmt::Debug for SampleProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleProducer")
            .field("capacity", &self.capacity)
            .field("free", &self.free())
            .field("dropped", &self.dropped)
            .finish()
    }
}

impl std::fmt::Debug for SampleConsumer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleConsumer")
            .field("capacity", &self.capacity)
            .field("available", &self.available())
            .finish()
    }
}
