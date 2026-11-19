//! Estado de audio por sesión: PCM16 pasa sin tocar y Opus se decodifica.
//!
//! El camino PCM16 es el contrato base de v1 y **no cambia**: [`SessionAudio::handle`]
//! devuelve exactamente las mismas muestras que trae el frame. Opus sólo se
//! decodifica si la sesión negoció `audio.opus-48k-mono`; si no, el payload se
//! descarta y se cuenta, como exige `docs/protocol.md` §8.3.
//!
//! Cuando faltan secuencias dentro de la ventana de reordenamiento (20 ms) se
//! generan tramas PLC con [`arl_codec::OpusDecoder::decode`]`(None)`. Un hueco
//! mayor o un cambio de época se trata como discontinuidad: se reinicia el
//! decodificador y no se inventan tramas. El estado del decodificador depende
//! del perfil (2.5, 5 o 10 ms); se conserva uno por perfil porque la cabecera
//! Opus puede cambiar de `sample_count` y el wrapper exige que la salida mida
//! exactamente una trama del perfil.

use arl_audio_core::{AudioPayload, OpusFrame};
use arl_codec::{CodecError, CodecProfile, OpusDecoder};

/// Ventana de reordenamiento del contrato (§9): 20 ms.
pub const REORDER_WINDOW_US: u32 = 20_000;

/// Muestras PCM de un frame de salida, en orden de reproducción.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedAudio {
    /// Época del frame que originó estas muestras.
    pub epoch: u32,
    /// Secuencia que corresponde a estas muestras (la del hueco, en PLC).
    pub seq: u32,
    /// Muestras PCM16 mono 48 kHz, sin remuestreo.
    pub samples: Vec<i16>,
}

/// Contadores observables de una sesión. Sólo cambian con el audio recibido.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionAudioStats {
    /// Frames PCM16 entregados tal cual.
    pub pcm16_frames: u64,
    /// Frames Opus decodificados (sin contar PLC).
    pub opus_frames: u64,
    /// Tramas PLC generadas para huecos dentro de la ventana.
    pub plc_frames: u64,
    /// Frames Opus descartados por no haber negociado la capacidad.
    pub dropped_unnegotiated_opus: u64,
    /// Frames con `seq` repetida (duplicados de red).
    pub dropped_duplicates: u64,
    /// Frames atrasados fuera de la ventana de reordenamiento.
    pub dropped_reordered: u64,
    /// Huecos mayores que la ventana o flags `discontinuity`.
    pub discontinuities: u64,
    /// Cambios de época observados.
    pub epoch_resets: u64,
    /// Paquetes que libopus no pudo decodificar.
    pub decode_errors: u64,
}

/// Estado de decodificación de una sesión autorizada.
#[derive(Debug)]
pub struct SessionAudio {
    opus_negotiated: bool,
    decoders: [Option<OpusDecoder>; 3],
    last_epoch: Option<u32>,
    last_seq: Option<u32>,
    last_profile: Option<CodecProfile>,
    stats: SessionAudioStats,
}

impl SessionAudio {
    /// Crea el estado. `opus_negotiated` debe reflejar la capacidad remota;
    /// sin ella los frames Opus se descartan en vez de decodificarse.
    pub fn new(opus_negotiated: bool) -> Self {
        Self {
            opus_negotiated,
            decoders: [None, None, None],
            last_epoch: None,
            last_seq: None,
            last_profile: None,
            stats: SessionAudioStats::default(),
        }
    }

    /// Actualiza la negociación. Al cambiar el formato se olvida la secuencia
    /// y se reinicia el decodificador: el emisor debe abrir una época nueva.
    pub fn set_opus_negotiated(&mut self, negotiated: bool) -> Result<(), CodecError> {
        if self.opus_negotiated != negotiated {
            self.opus_negotiated = negotiated;
            self.reset_decoders()?;
            self.last_epoch = None;
            self.last_seq = None;
            self.last_profile = None;
        }
        Ok(())
    }

    /// ¿La sesión negoció Opus?
    pub const fn opus_negotiated(&self) -> bool {
        self.opus_negotiated
    }

    /// Contadores acumulados.
    pub const fn stats(&self) -> SessionAudioStats {
        self.stats
    }

    /// Procesa un datagrama ya parseado y devuelve las muestras a reproducir.
    ///
    /// PCM16 devuelve una única entrada con las mismas muestras del frame.
    /// Opus puede devolver varias: una PLC por cada hueco dentro de la ventana
    /// más el frame actual. Un frame descartado no produce muestras.
    pub fn handle(&mut self, payload: AudioPayload) -> Result<Vec<DecodedAudio>, CodecError> {
        match payload {
            AudioPayload::Pcm16(frame) => {
                self.stats.pcm16_frames += 1;
                Ok(vec![DecodedAudio {
                    epoch: frame.epoch(),
                    seq: frame.seq(),
                    samples: frame.samples().to_vec(),
                }])
            }
            AudioPayload::Opus(frame) => self.handle_opus(frame),
        }
    }

    fn handle_opus(&mut self, frame: OpusFrame) -> Result<Vec<DecodedAudio>, CodecError> {
        if !self.opus_negotiated {
            self.stats.dropped_unnegotiated_opus += 1;
            return Ok(Vec::new());
        }
        let profile = match CodecProfile::from_frame_samples(frame.sample_count() as usize) {
            Ok(profile) => profile,
            Err(_) => {
                self.stats.decode_errors += 1;
                return Ok(Vec::new());
            }
        };
        let epoch = frame.epoch();
        let seq = frame.seq();
        let epoch_changed = self.last_epoch.is_some_and(|last| last != epoch);
        // Época atrasada: frame de una época ya superada que llegó tarde. No
        // se decodifica ni se reinicia el estado de la época vigente.
        if epoch_changed
            && self
                .last_epoch
                .is_some_and(|last| epoch.wrapping_sub(last) > u32::MAX / 2)
        {
            self.stats.dropped_reordered += 1;
            return Ok(Vec::new());
        }
        let discontinuity = frame.flags().is_discontinuity();

        if epoch_changed || discontinuity {
            self.reset_decoders()?;
            if epoch_changed {
                self.stats.epoch_resets += 1;
            }
            if discontinuity {
                self.stats.discontinuities += 1;
            }
            self.last_epoch = Some(epoch);
            self.last_seq = Some(seq);
            self.last_profile = Some(profile);
            let samples = self.decode_packet(profile, frame.packet())?;
            self.stats.opus_frames += 1;
            return Ok(vec![DecodedAudio {
                epoch,
                seq,
                samples,
            }]);
        }

        let mut out = Vec::new();
        if let (Some(last_seq), Some(last_profile)) = (self.last_seq, self.last_profile) {
            let delta = seq.wrapping_sub(last_seq);
            if delta == 0 {
                self.stats.dropped_duplicates += 1;
                return Ok(out);
            }
            if delta > u32::MAX / 2 {
                // Secuencia atrasada: llegó después de frames más nuevos y
                // fuera de la ventana de 20 ms.
                self.stats.dropped_reordered += 1;
                return Ok(out);
            }
            let missing = delta - 1;
            if missing <= max_plc_frames(last_profile.frame_duration_us()) {
                for offset in 0..missing {
                    let samples = self.decode_plc(last_profile)?;
                    out.push(DecodedAudio {
                        epoch,
                        seq: last_seq.wrapping_add(offset + 1),
                        samples,
                    });
                    self.stats.plc_frames += 1;
                }
            } else {
                // Hueco demasiado largo para PLC: se reinicia el estado y se
                // entrega el frame actual sin inventar audio.
                self.reset_decoders()?;
                self.stats.discontinuities += 1;
            }
        }

        let samples = self.decode_packet(profile, frame.packet())?;
        self.stats.opus_frames += 1;
        self.last_epoch = Some(epoch);
        self.last_seq = Some(seq);
        self.last_profile = Some(profile);
        out.push(DecodedAudio {
            epoch,
            seq,
            samples,
        });
        Ok(out)
    }

    fn decode_packet(
        &mut self,
        profile: CodecProfile,
        packet: &[u8],
    ) -> Result<Vec<i16>, CodecError> {
        match self.decoder(profile)?.decode(Some(packet)) {
            Ok(samples) => Ok(samples),
            Err(error) => {
                self.stats.decode_errors += 1;
                Err(error)
            }
        }
    }

    fn decode_plc(&mut self, profile: CodecProfile) -> Result<Vec<i16>, CodecError> {
        match self.decoder(profile)?.decode(None) {
            Ok(samples) => Ok(samples),
            Err(error) => {
                self.stats.decode_errors += 1;
                Err(error)
            }
        }
    }

    fn decoder(&mut self, profile: CodecProfile) -> Result<&mut OpusDecoder, CodecError> {
        let index = profile_index(profile);
        if self.decoders[index].is_none() {
            self.decoders[index] = Some(OpusDecoder::new(profile)?);
        }
        match self.decoders[index].as_mut() {
            Some(decoder) => Ok(decoder),
            None => unreachable!("el decodificador se inicializó arriba"),
        }
    }

    fn reset_decoders(&mut self) -> Result<(), CodecError> {
        for decoder in self.decoders.iter_mut().flatten() {
            decoder.reset()?;
        }
        Ok(())
    }
}

const fn profile_index(profile: CodecProfile) -> usize {
    match profile {
        CodecProfile::LowDelay2p5Ms => 0,
        CodecProfile::LowDelay5Ms => 1,
        CodecProfile::LowDelay10Ms => 2,
    }
}

/// Huecos máximos ocultables dentro de la ventana de reordenamiento.
fn max_plc_frames(duration_us: u32) -> u32 {
    if duration_us == 0 {
        return 0;
    }
    (REORDER_WINDOW_US / duration_us).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arl_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};
    use arl_codec::OpusEncoder;

    fn opus_frame(epoch: u32, seq: u32, flags: Flags, packet: &[u8]) -> OpusFrame {
        OpusFrame::new(epoch, seq, flags, SAMPLE_COUNT as u16, packet).expect("frame Opus")
    }

    fn packet(encoder: &mut OpusEncoder) -> Vec<u8> {
        let mut pcm = [0i16; SAMPLE_COUNT];
        for (index, sample) in pcm.iter_mut().enumerate() {
            *sample = (index as i16).wrapping_mul(97);
        }
        encoder.encode(&pcm).expect("encode")
    }

    #[test]
    fn pcm16_passes_through_byte_identical() {
        let mut samples = [0i16; SAMPLE_COUNT];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = (index as i16).wrapping_sub(120);
        }
        let frame = AudioFrame::new(7, 9, Flags::NONE, samples).expect("frame PCM16");
        let mut state = SessionAudio::new(false);
        let out = state
            .handle(AudioPayload::Pcm16(frame.clone()))
            .expect("passthrough");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].epoch, 7);
        assert_eq!(out[0].seq, 9);
        assert_eq!(out[0].samples, samples);
        let rebuilt = AudioFrame::new(
            out[0].epoch,
            out[0].seq,
            Flags::NONE,
            out[0].samples.as_slice().try_into().expect("240 muestras"),
        )
        .expect("frame reconstruido");
        assert_eq!(rebuilt.encode(), frame.encode(), "bytes PCM16 idénticos");
        assert_eq!(state.stats().pcm16_frames, 1);
    }

    #[test]
    fn opus_roundtrip_yields_one_frame_of_profile_samples() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let packet = packet(&mut encoder);
        let mut state = SessionAudio::new(true);
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &packet)))
            .expect("decode");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].epoch, 1);
        assert_eq!(out[0].seq, 0);
        assert_eq!(out[0].samples.len(), SAMPLE_COUNT);
        assert!(out[0].samples.iter().any(|sample| *sample != 0));
        assert_eq!(state.stats().opus_frames, 1);
        assert_eq!(state.stats().plc_frames, 0);
    }

    #[test]
    fn seq_gap_generates_bounded_plc_with_profile_length() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let first = packet(&mut encoder);
        let fourth = packet(&mut encoder);
        let mut state = SessionAudio::new(true);
        state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &first)))
            .expect("primero");
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 3, Flags::NONE, &fourth)))
            .expect("hueco");
        assert_eq!(out.len(), 3, "dos PLC y el frame actual");
        assert_eq!(out[0].seq, 1);
        assert_eq!(out[1].seq, 2);
        assert_eq!(out[2].seq, 3);
        assert!(out[..2]
            .iter()
            .all(|frame| frame.samples.len() == SAMPLE_COUNT));
        assert_eq!(state.stats().plc_frames, 2);
        assert_eq!(state.stats().opus_frames, 2);
    }

    #[test]
    fn gap_beyond_reorder_window_resets_instead_of_plc() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let first = packet(&mut encoder);
        let later = packet(&mut encoder);
        let mut state = SessionAudio::new(true);
        state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &first)))
            .expect("primero");
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 40, Flags::NONE, &later)))
            .expect("hueco largo");
        assert_eq!(out.len(), 1, "sin PLC fuera de la ventana");
        assert_eq!(out[0].seq, 40);
        assert_eq!(state.stats().plc_frames, 0);
        assert_eq!(state.stats().discontinuities, 1);
    }

    #[test]
    fn epoch_change_resets_decoder_and_sequence() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let first = packet(&mut encoder);
        let second = packet(&mut encoder);
        let mut state = SessionAudio::new(true);
        state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &first)))
            .expect("época 1");
        let out = state
            .handle(AudioPayload::Opus(opus_frame(
                2,
                0,
                Flags::DISCONTINUITY,
                &second,
            )))
            .expect("época 2");
        assert_eq!(out.len(), 1, "la época nueva no arrastra PLC");
        assert_eq!(out[0].epoch, 2);
        assert_eq!(state.stats().epoch_resets, 1);
        assert_eq!(state.stats().plc_frames, 0);
    }

    #[test]
    fn opus_is_rejected_until_negotiated() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let packet = packet(&mut encoder);
        let mut state = SessionAudio::new(false);
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &packet)))
            .expect("descartado sin error");
        assert!(out.is_empty());
        assert_eq!(state.stats().dropped_unnegotiated_opus, 1);
        assert_eq!(state.stats().opus_frames, 0);

        state.set_opus_negotiated(true).expect("negociación");
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &packet)))
            .expect("ya negociado");
        assert_eq!(out.len(), 1);
        assert_eq!(state.stats().opus_frames, 1);
    }

    #[test]
    fn duplicate_sequence_is_dropped() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let first = packet(&mut encoder);
        let second = packet(&mut encoder);
        let mut state = SessionAudio::new(true);
        state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &first)))
            .expect("primero");
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 0, Flags::NONE, &second)))
            .expect("duplicado");
        assert!(out.is_empty());
        assert_eq!(state.stats().dropped_duplicates, 1);
    }

    #[test]
    fn late_frame_from_old_epoch_is_dropped_without_resetting() {
        let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
        let first = packet(&mut encoder);
        let second = packet(&mut encoder);
        let mut state = SessionAudio::new(true);
        state
            .handle(AudioPayload::Opus(opus_frame(2, 0, Flags::NONE, &first)))
            .expect("época vigente");
        let out = state
            .handle(AudioPayload::Opus(opus_frame(1, 5, Flags::NONE, &second)))
            .expect("época atrasada");
        assert!(out.is_empty(), "la época vieja no se reproduce");
        assert_eq!(state.stats().dropped_reordered, 1);
        assert_eq!(state.stats().epoch_resets, 0);
    }
}
