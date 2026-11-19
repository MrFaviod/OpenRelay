//! Retorno de audio Linux→Android (F8/T24, D-08).
//!
//! Aquí vive la lógica pura y testeable del retorno: la decisión
//! [`should_send`] y el emisor [`ReturnEmitter`], que toma bloques PCM16LE mono
//! 48 kHz de una fuente [`PcmSource`] y arma frames `audio-frame/v1` con el
//! encoder ya existente de `openrelay-audio-core`. Este módulo no reimplementa el
//! wire: construye `AudioFrame` y deja que su `encode` ponga la cabecera.
//!
//! El retorno es un flujo aparte del micrófono Android→Linux: cada emisor
//! lleva su propia secuencia y su propia época, y no comparte `seq` ni
//! datagrama con el flujo de entrada. Compilar el daemon sin la feature
//! `return-audio` deja este módulo fuera del binario y el comportamiento del
//! proceso no cambia.

use openrelay_audio_core::{AudioFrame, Flags, FrameError, Samples, SAMPLE_COUNT};
use openrelay_proto::control::Feature;

/// Feature del contrato que habilita el retorno PCM16LE mono 48 kHz.
pub const RETURN_FEATURE: Feature = Feature::AudioReturnPcm48kMono;

/// Features locales **efectivas** para la negociación del retorno.
///
/// `RETURN_FEATURE` sólo cuenta si `audio.return_enabled` vale `true`: el
/// daemon no puede anunciar como propia una capacidad que su configuración
/// tiene apagada. El perfil PCM16 de entrada sigue siempre.
pub fn effective_local_features(return_enabled: bool) -> Vec<Feature> {
    let mut features = vec![Feature::AudioPcm16_48kMono];
    if return_enabled {
        features.push(RETURN_FEATURE);
    }
    features
}

/// Época del flujo de retorno.
///
/// Es un flujo distinto del micrófono: no reutiliza la época de ningún flujo
/// entrante. Debe ser distinta de cero, como exige el contrato.
pub const RETURN_EPOCH: u32 = 1;

/// Decide si el daemon debe emitir audio de retorno.
///
/// Sólo con las tres condiciones: el peer anunció `audio.return-pcm16-48k-mono`,
/// el extremo local también, y la clave local `audio.return_enabled` es `true`.
/// Cualquier otra combinación deja el flujo apagado.
pub fn should_send(
    peer_features: &[Feature],
    local_features: &[Feature],
    return_enabled: bool,
) -> bool {
    return_enabled
        && peer_features.contains(&RETURN_FEATURE)
        && local_features.contains(&RETURN_FEATURE)
}

/// Fuente de bloques PCM16LE mono 48 kHz.
///
/// El método es el único contrato con el backend: llena `out` con hasta
/// `out.len()` muestras y devuelve cuántas escribió. Devolver `0` significa
/// "sin datos ahora"; el emisor conserva el bloque parcial y espera.
pub trait PcmSource {
    /// Llena `out` y devuelve el número de muestras escritas (`<= out.len()`).
    fn read(&mut self, out: &mut [i16]) -> usize;
}

/// Emisor de un flujo de retorno.
///
/// Junta muestras de la fuente hasta completar un bloque del contrato y lo
/// publica como `AudioFrame` con secuencia propia. Con `enabled` en `false` no
/// lee la fuente ni produce un solo frame: ese es el corte que impone
/// [`should_send`].
pub struct ReturnEmitter<S: PcmSource> {
    source: S,
    epoch: u32,
    next_seq: u32,
    enabled: bool,
    block: Samples,
    pending: usize,
}

impl<S: PcmSource> ReturnEmitter<S> {
    /// Crea el emisor. Rechaza una época cero con el mismo criterio que el
    /// contrato, sin llegar a construir el primer frame.
    pub fn new(source: S, epoch: u32, enabled: bool) -> Result<Self, FrameError> {
        AudioFrame::new(epoch, 0, Flags::NONE, [0; SAMPLE_COUNT])?;
        Ok(Self {
            source,
            epoch,
            next_seq: 0,
            enabled,
            block: [0; SAMPLE_COUNT],
            pending: 0,
        })
    }

    /// `true` si el emisor puede producir frames.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Fuente de la que el emisor toma muestras.
    pub fn source(&self) -> &S {
        &self.source
    }

    /// Secuencia que llevará el próximo frame.
    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }

    /// Devuelve el próximo frame completo **sin consumir** su secuencia, o
    /// `None` si aún no hay bloque o el emisor está apagado.
    ///
    /// El llamador sólo debe llamar a [`Self::commit`] si el envío salió bien:
    /// así un fallo de transporte no abre un hueco falso en `seq`.
    pub fn peek(&mut self) -> Option<AudioFrame> {
        if !self.enabled {
            return None;
        }
        while self.pending < SAMPLE_COUNT {
            let read = self.source.read(&mut self.block[self.pending..]);
            if read == 0 {
                break;
            }
            // Una fuente que devuelva más de lo que cabe no puede hacer
            // paniquear al emisor: se recorta al hueco real.
            let room = SAMPLE_COUNT - self.pending;
            self.pending += read.min(room);
        }
        if self.pending < SAMPLE_COUNT {
            return None;
        }
        self.pending = 0;
        AudioFrame::new(self.epoch, self.next_seq, Flags::NONE, self.block).ok()
    }

    /// Da por enviado el frame que devolvió [`Self::peek`] y avanza `seq`.
    pub fn commit(&mut self) {
        self.next_seq = self.next_seq.wrapping_add(1);
    }

    /// Devuelve el próximo frame completo y consume su secuencia.
    ///
    /// Atajo de [`Self::peek`] + [`Self::commit`] para quien ya sabe que el
    /// frame se entrega. La secuencia es creciente dentro del flujo y el wrap
    /// es válido, como en el contrato.
    pub fn pull(&mut self) -> Option<AudioFrame> {
        let frame = self.peek()?;
        self.commit();
        Some(frame)
    }
}

/// Adaptador trivial al trait [`PcmSource`]: el monitor de PipeWire ya entrega
/// S16LE mono 48 kHz desde su anillo interno.
impl PcmSource for openrelay_audio_linux::MonitorSource {
    fn read(&mut self, out: &mut [i16]) -> usize {
        self.read_samples(out)
    }
}
