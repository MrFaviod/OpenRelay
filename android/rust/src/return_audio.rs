//! Cola acotada del retorno Linux→Android (`docs/protocol.md` §8.4).
//!
//! El retorno reutiliza el frame `type=1` (PCM16LE, 48 kHz, mono, 240
//! muestras). El puente no decodifica a mano: el transporte ya entrega el
//! [`AudioPayload`] tipado con el decodificador del contrato
//! (`openrelay_audio_core::AudioPayload::decode`), y aquí sólo se copian las
//! muestras a una cola separada del flujo de micrófono.
//!
//! La cola está apagada por defecto: sin el anuncio
//! `audio.return-pcm16-48k-mono` no se encola nada. Su capacidad es fija y lo
//! viejo se descarta; nunca crece por ráfaga.

use std::collections::VecDeque;

use openrelay_audio_core::{AudioPayload, SAMPLE_COUNT};

/// Frames de 5 ms que caben en la cola (20 ms). Al llenarse se tira el más
/// viejo: el retorno no se recupera por retransmisión.
pub(crate) const RETURN_CAP: usize = 4;

/// Cola de frames de retorno ya decodificados, separada del micrófono.
#[derive(Debug)]
pub(crate) struct ReturnQueue {
    frames: VecDeque<[i16; SAMPLE_COUNT]>,
    enabled: bool,
}

impl ReturnQueue {
    /// Cola vacía y apagada: sin anuncio no se encola nada.
    pub(crate) fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            enabled: false,
        }
    }

    /// Enciende o apaga el retorno. Al apagarlo se vacía lo pendiente.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.frames.clear();
        }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Encola un payload recibido del servidor. Sólo acepta `type=1` y sólo
    /// con el retorno anunciado; cualquier otra cosa se descarta.
    pub(crate) fn push(&mut self, payload: AudioPayload) -> bool {
        if !self.enabled {
            return false;
        }
        let AudioPayload::Pcm16(frame) = payload else {
            return false;
        };
        if frame.samples().len() != SAMPLE_COUNT {
            return false;
        }
        let mut block = [0i16; SAMPLE_COUNT];
        block.copy_from_slice(frame.samples());
        if self.frames.len() >= RETURN_CAP {
            self.frames.pop_front();
        }
        self.frames.push_back(block);
        true
    }

    /// Copia el frame más viejo en `dst`. Devuelve las muestras copiadas
    /// ([`SAMPLE_COUNT`]) o `0` si la cola está vacía.
    pub(crate) fn read_into(&mut self, dst: &mut [i16]) -> usize {
        if dst.len() < SAMPLE_COUNT {
            return 0;
        }
        match self.frames.pop_front() {
            Some(block) => {
                dst[..SAMPLE_COUNT].copy_from_slice(&block);
                SAMPLE_COUNT
            }
            None => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.frames.len()
    }
}

impl Default for ReturnQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrelay_audio_core::{AudioFrame, Flags};

    /// Frame `type=1` válido según el contrato: se codifica y se decodifica
    /// con el decodificador existente antes de entrar en la cola.
    fn wire_frame(first: i16) -> AudioPayload {
        let mut samples = [0i16; SAMPLE_COUNT];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = first.wrapping_add(index as i16);
        }
        let frame = AudioFrame::new(1, 0, Flags::NONE, samples).expect("frame type=1");
        let wire = frame.encode();
        assert_eq!(wire.len(), openrelay_audio_core::DATAGRAM_LEN);
        AudioPayload::decode(&wire).expect("el frame de retorno no decodifica")
    }

    /// El frame válido entra en la cola y se lee entero, muestra a muestra.
    #[test]
    fn valid_type1_frame_is_queued_and_read() {
        let payload = wire_frame(100);
        let mut queue = ReturnQueue::new();
        queue.set_enabled(true);
        assert!(queue.push(payload));

        let mut dst = [0i16; SAMPLE_COUNT];
        assert_eq!(queue.read_into(&mut dst), SAMPLE_COUNT);
        assert_eq!(dst[0], 100);
        assert_eq!(
            dst[SAMPLE_COUNT - 1],
            100i16.wrapping_add((SAMPLE_COUNT - 1) as i16)
        );
        assert_eq!(queue.read_into(&mut dst), 0, "la cola se vació al leer");
    }

    /// La cola es acotada: al llenarse descarta el frame más viejo.
    #[test]
    fn bounded_queue_drops_the_oldest() {
        let mut queue = ReturnQueue::new();
        queue.set_enabled(true);
        for index in 0..(RETURN_CAP + 2) {
            assert!(queue.push(wire_frame((index as i16) * 100)));
        }
        assert_eq!(queue.len(), RETURN_CAP);

        let mut dst = [0i16; SAMPLE_COUNT];
        assert_eq!(queue.read_into(&mut dst), SAMPLE_COUNT);
        assert_eq!(dst[0], 200, "el más viejo (0) debía estar descartado");
    }

    /// Sin el anuncio de la feature no se encola nada, aunque llegue el frame.
    #[test]
    fn without_advertisement_nothing_is_queued() {
        let mut queue = ReturnQueue::new();
        assert!(!queue.push(wire_frame(7)));
        assert_eq!(queue.len(), 0);
        let mut dst = [0i16; SAMPLE_COUNT];
        assert_eq!(queue.read_into(&mut dst), 0);

        // Apagarlo de nuevo también vacía lo pendiente.
        queue.set_enabled(true);
        assert!(queue.push(wire_frame(7)));
        queue.set_enabled(false);
        assert_eq!(queue.len(), 0);
    }
}
