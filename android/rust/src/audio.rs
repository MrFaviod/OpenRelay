//! Selección de códec del envío de audio del puente.
//!
//! PCM16 es el perfil obligatorio de v1 y el camino por defecto: con
//! [`CodecChoice::Pcm16`] el puente emite exactamente un [`AudioFrame`]
//! (508 B, `type=1`), byte a byte igual al contrato. Opus sólo se elige si
//! Java pidió un perfil y el servidor anunció `audio.opus-48k-mono` en su
//! `capabilities`; si falta cualquiera de los dos, la sesión se queda en
//! PCM16. Ningún `OpusFrame` se reetiqueta como PCM16 ni al revés.

use openrelay_audio_core::{
    AudioFormat, AudioFrame, AudioPayload, Flags, OpusFrame, FEATURE_AUDIO_OPUS,
    FEATURE_AUDIO_PCM16, SAMPLE_COUNT,
};
use openrelay_codec::{CodecProfile, OpusEncoder};

/// Bitrate del codificador Opus del puente (bits/s).
pub(crate) const OPUS_BITRATE_BPS: u32 = 64_000;

/// El `SessionClient` expone `send_audio_payload(&AudioPayload)`, que pone en
/// el cable tanto `type=1` como `type=2`, así que lo negociado se puede emitir.
/// Si ese envío faltara en otro árbol del transporte, poner esto en `false`
/// deja la sesión en PCM16 sin tocar el resto.
pub(crate) const TRANSPORT_SENDS_OPUS: bool = true;

/// Perfil de envío pedido por Java. `0` = PCM16, el contrato por defecto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodecChoice {
    /// Frame PCM16LE fijo de 5 ms (`type=1`).
    Pcm16,
    /// Frame Opus con el perfil indicado (`type=2`).
    Opus(CodecProfile),
}

impl CodecChoice {
    /// Traduce el `int` de `setCodec`: `0` PCM16, `1` Opus 5 ms (el perfil
    /// por defecto), `2` Opus 2,5 ms, `3` Opus 10 ms.
    pub(crate) fn from_jint(profile: i32) -> Result<Self, String> {
        match profile {
            0 => Ok(Self::Pcm16),
            1 => Ok(Self::Opus(CodecProfile::LowDelay5Ms)),
            2 => Ok(Self::Opus(CodecProfile::LowDelay2p5Ms)),
            3 => Ok(Self::Opus(CodecProfile::LowDelay10Ms)),
            other => Err(format!("perfil de códec desconocido: {other}")),
        }
    }

    /// Perfil Opus, o `None` si el perfil activo es PCM16.
    pub(crate) const fn profile(self) -> Option<CodecProfile> {
        match self {
            Self::Pcm16 => None,
            Self::Opus(profile) => Some(profile),
        }
    }
}

/// Features que el puente declara según el perfil pedido: PCM16 siempre, Opus
/// sólo si hay perfil configurado.
pub(crate) fn local_features(choice: CodecChoice) -> Vec<&'static str> {
    match choice {
        CodecChoice::Pcm16 => vec![FEATURE_AUDIO_PCM16],
        CodecChoice::Opus(_) => vec![FEATURE_AUDIO_PCM16, FEATURE_AUDIO_OPUS],
    }
}

/// Formato que resulta de la negociación entre el perfil pedido y las features
/// que el servidor anunció, sin mirar lo que el transporte sabe enviar.
pub(crate) fn negotiated_format(choice: CodecChoice, server: &[String]) -> AudioFormat {
    if choice.profile().is_none() {
        return AudioFormat::Pcm16;
    }
    let remote: Vec<&str> = server.iter().map(String::as_str).collect();
    AudioFormat::negotiate(&local_features(choice), &remote)
}

/// Formato que el puente puede emitir de verdad: lo negociado, limitado por lo
/// que el transporte sabe enviar y por haber anunciado ya las features locales.
/// Es la única puerta que consulta el envío.
pub(crate) fn selected_format(
    choice: CodecChoice,
    server: &[String],
    local_advertised: bool,
) -> AudioFormat {
    match negotiated_format(choice, server) {
        AudioFormat::Opus if !TRANSPORT_SENDS_OPUS || !local_advertised => AudioFormat::Pcm16,
        format => format,
    }
}

/// Codifica bloques PCM16 del perfil activo al frame que corresponda.
///
/// Guarda la época y la secuencia del flujo: el llamador entrega muestras y
/// recibe un [`AudioPayload`] listo para serializar. Un cambio de perfil abre
/// época nueva y reinicia la secuencia, para que el receptor no mezcle estados
/// de códec dentro de la misma secuencia.
#[derive(Debug)]
pub(crate) struct AudioSender {
    choice: CodecChoice,
    encoder: Option<OpusEncoder>,
    /// Época del flujo; nunca `0`.
    epoch: u32,
    seq: u32,
}

impl AudioSender {
    /// Envío en PCM16, época `1`, secuencia `0`.
    pub(crate) fn new() -> Self {
        Self {
            choice: CodecChoice::Pcm16,
            encoder: None,
            epoch: 1,
            seq: 0,
        }
    }

    /// Perfil activo.
    pub(crate) const fn choice(&self) -> CodecChoice {
        self.choice
    }

    /// Fija el perfil y prepara su codificador. Si el perfil cambia de verdad,
    /// abre época nueva y reinicia la secuencia.
    pub(crate) fn set_choice(&mut self, choice: CodecChoice) -> Result<(), String> {
        let encoder = match choice {
            CodecChoice::Pcm16 => None,
            CodecChoice::Opus(profile) => Some(
                OpusEncoder::new(profile, OPUS_BITRATE_BPS)
                    .map_err(|error| format!("códec Opus: {error}"))?,
            ),
        };
        if choice != self.choice {
            // `wrapping_add` puede dar 0 tras u32::MAX; la época 0 no existe.
            self.epoch = self.epoch.wrapping_add(1).max(1);
            self.seq = 0;
            self.encoder = encoder;
        } else {
            self.encoder = encoder;
        }
        self.choice = choice;
        Ok(())
    }

    /// Codifica un bloque de muestras en el `format` ya seleccionado.
    ///
    /// PCM16 exige exactamente [`SAMPLE_COUNT`] muestras (5 ms). Opus exige
    /// exactamente las muestras del perfil activo. Una longitud que no
    /// corresponda es un error: no se rellena ni se recorta.
    pub(crate) fn encode(
        &mut self,
        format: AudioFormat,
        samples: &[i16],
    ) -> Result<AudioPayload, String> {
        let seq = self.seq;
        let payload = match format {
            AudioFormat::Pcm16 => {
                if samples.len() != SAMPLE_COUNT {
                    return Err(format!(
                        "PCM16 espera {SAMPLE_COUNT} muestras, hay {}",
                        samples.len()
                    ));
                }
                let mut block = [0i16; SAMPLE_COUNT];
                block.copy_from_slice(samples);
                let frame = AudioFrame::new(self.epoch, seq, Flags::NONE, block)
                    .map_err(|error| error.to_string())?;
                AudioPayload::Pcm16(frame)
            }
            AudioFormat::Opus => {
                let profile = self.choice.profile().ok_or("Opus sin perfil configurado")?;
                if samples.len() != profile.samples_per_frame() {
                    return Err(format!(
                        "Opus de {} muestras por trama, hay {}",
                        profile.samples_per_frame(),
                        samples.len()
                    ));
                }
                let encoder = self.encoder.as_mut().ok_or("codificador Opus ausente")?;
                let packet = encoder.encode(samples).map_err(|error| error.to_string())?;
                let frame =
                    OpusFrame::new(self.epoch, seq, Flags::NONE, samples.len() as u16, &packet)
                        .map_err(|error| error.to_string())?;
                AudioPayload::Opus(frame)
            }
        };
        self.seq = self.seq.wrapping_add(1);
        Ok(payload)
    }
}

impl Default for AudioSender {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrelay_codec::OpusDecoder;

    fn features(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    /// Sin perfil configurado el envío es PCM16, anuncie el servidor lo que
    /// anuncie.
    #[test]
    fn pcm16_is_default_even_if_server_advertises_opus() {
        let server = features(&[FEATURE_AUDIO_OPUS]);
        assert_eq!(
            negotiated_format(CodecChoice::Pcm16, &server),
            AudioFormat::Pcm16
        );
        assert_eq!(
            selected_format(CodecChoice::Pcm16, &server, true),
            AudioFormat::Pcm16
        );
    }

    /// Con perfil Opus y el servidor anunciando la feature, la negociación da
    /// Opus.
    #[test]
    fn opus_when_both_ends_advertise_it() {
        let server = features(&[FEATURE_AUDIO_PCM16, FEATURE_AUDIO_OPUS]);
        assert_eq!(
            negotiated_format(CodecChoice::Opus(CodecProfile::LowDelay5Ms), &server),
            AudioFormat::Opus
        );
    }

    /// Con perfil Opus pero sin la feature del servidor, se cae a PCM16.
    #[test]
    fn pcm16_when_server_does_not_advertise_opus() {
        let server = features(&[FEATURE_AUDIO_PCM16]);
        assert_eq!(
            negotiated_format(CodecChoice::Opus(CodecProfile::LowDelay5Ms), &server),
            AudioFormat::Pcm16
        );
        assert_eq!(
            negotiated_format(CodecChoice::Opus(CodecProfile::LowDelay5Ms), &[]),
            AudioFormat::Pcm16
        );
    }

    /// La selección efectiva sigue a `TRANSPORT_SENDS_OPUS`: con el envío de
    /// `AudioPayload` disponible, lo negociado se emite tal cual.
    #[test]
    fn selected_format_respects_transport_support() {
        let server = features(&[FEATURE_AUDIO_OPUS]);
        let choice = CodecChoice::Opus(CodecProfile::LowDelay5Ms);
        let expected = if TRANSPORT_SENDS_OPUS {
            AudioFormat::Opus
        } else {
            AudioFormat::Pcm16
        };
        assert_eq!(selected_format(choice, &server, true), expected);
    }

    /// Aunque ambos extremos anuncien Opus, no se emite hasta que nuestras
    /// `capabilities` salieron: el daemon descarta Opus que no fue anunciado.
    #[test]
    fn selected_format_waits_for_local_capabilities() {
        let server = features(&[FEATURE_AUDIO_OPUS]);
        let choice = CodecChoice::Opus(CodecProfile::LowDelay5Ms);
        assert_eq!(selected_format(choice, &server, false), AudioFormat::Pcm16);
        if TRANSPORT_SENDS_OPUS {
            assert_eq!(selected_format(choice, &server, true), AudioFormat::Opus);
        }
    }

    /// El `int` de `setCodec` mapea 0 a PCM16 y 1..3 a los perfiles Opus.
    #[test]
    fn set_codec_arguments_map_to_profiles() {
        assert_eq!(CodecChoice::from_jint(0), Ok(CodecChoice::Pcm16));
        assert_eq!(
            CodecChoice::from_jint(1),
            Ok(CodecChoice::Opus(CodecProfile::LowDelay5Ms))
        );
        assert_eq!(
            CodecChoice::from_jint(2),
            Ok(CodecChoice::Opus(CodecProfile::LowDelay2p5Ms))
        );
        assert_eq!(
            CodecChoice::from_jint(3),
            Ok(CodecChoice::Opus(CodecProfile::LowDelay10Ms))
        );
        assert!(CodecChoice::from_jint(4).is_err());
    }

    /// El camino PCM16 emite el mismo datagrama que el contrato: 508 B y
    /// `type=1`, muestras little-endian sin tocar.
    #[test]
    fn pcm16_frame_is_byte_identical() {
        let mut samples = [0i16; SAMPLE_COUNT];
        for (index, sample) in samples.iter_mut().enumerate() {
            *sample = (index as i16) * 7 - 500;
        }
        let mut sender = AudioSender::new();
        let payload = sender.encode(AudioFormat::Pcm16, &samples).expect("PCM16");
        let AudioPayload::Pcm16(frame) = &payload else {
            panic!("el perfil PCM16 no produjo un AudioFrame");
        };
        let (wire, len) = payload.encode();
        let reference = openrelay_audio_core::AudioFrame::new(1, 0, Flags::NONE, samples)
            .expect("frame de referencia")
            .encode();
        assert_eq!(len, openrelay_audio_core::DATAGRAM_LEN);
        assert_eq!(&wire[..len], &reference[..], "PCM16 cambió en el cable");
        assert_eq!(wire[0], openrelay_audio_core::TYPE_AUDIO);
        assert_eq!(frame.samples(), &samples);
    }

    /// El camino Opus produce `type=2` con 240 muestras declaradas y un paquete
    /// que el decodificador del crate recupera entero.
    #[test]
    fn opus_frame_declares_five_ms_and_roundtrips() {
        let mut samples = [0i16; SAMPLE_COUNT];
        for (index, sample) in samples.iter_mut().enumerate() {
            let phase = index as f32 / SAMPLE_COUNT as f32;
            *sample = ((phase * core::f32::consts::TAU).sin() * 8_000.0) as i16;
        }
        let mut sender = AudioSender::new();
        sender
            .set_choice(CodecChoice::Opus(CodecProfile::LowDelay5Ms))
            .expect("perfil Opus 5 ms");
        let payload = sender.encode(AudioFormat::Opus, &samples).expect("Opus");
        let AudioPayload::Opus(frame) = &payload else {
            panic!("el perfil Opus no produjo un OpusFrame");
        };
        assert_eq!(frame.sample_count(), 240);
        assert_eq!(frame.duration_us(), 5_000);
        let (wire, len) = payload.encode();
        assert_eq!(wire[0], openrelay_audio_core::TYPE_AUDIO_OPUS);
        assert_eq!(len, openrelay_audio_core::HEADER_LEN + frame.packet().len());
        let decoded = openrelay_audio_core::AudioPayload::decode(&wire[..len]).expect("decode");
        let AudioPayload::Opus(decoded) = decoded else {
            panic!("el datagrama Opus no volvió como Opus");
        };
        let mut decoder = OpusDecoder::new(CodecProfile::LowDelay5Ms).expect("decoder");
        let pcm = decoder.decode(Some(decoded.packet())).expect("decode opus");
        assert_eq!(pcm.len(), SAMPLE_COUNT);
    }

    /// Una longitud que no corresponde al perfil es un error, no un relleno.
    #[test]
    fn wrong_block_length_is_rejected() {
        let mut sender = AudioSender::new();
        assert!(sender.encode(AudioFormat::Pcm16, &[0i16; 120]).is_err());
        sender
            .set_choice(CodecChoice::Opus(CodecProfile::LowDelay2p5Ms))
            .expect("perfil 2,5 ms");
        assert!(sender
            .encode(AudioFormat::Opus, &[0i16; SAMPLE_COUNT])
            .is_err());
    }

    /// La secuencia avanza sólo con envíos válidos, y un cambio de perfil abre
    /// época nueva con la secuencia a cero.
    #[test]
    fn sequence_and_epoch_track_profile_changes() {
        let mut sender = AudioSender::new();
        let first = sender
            .encode(AudioFormat::Pcm16, &[1i16; SAMPLE_COUNT])
            .expect("1");
        let second = sender
            .encode(AudioFormat::Pcm16, &[2i16; SAMPLE_COUNT])
            .expect("2");
        let (AudioPayload::Pcm16(first), AudioPayload::Pcm16(second)) = (first, second) else {
            panic!("PCM16 esperado");
        };
        assert_eq!((first.epoch(), first.seq()), (1, 0));
        assert_eq!((second.epoch(), second.seq()), (1, 1));
        assert!(sender.encode(AudioFormat::Pcm16, &[0i16; 10]).is_err());

        sender
            .set_choice(CodecChoice::Opus(CodecProfile::LowDelay5Ms))
            .expect("perfil Opus");
        let opus = sender
            .encode(AudioFormat::Opus, &[0i16; SAMPLE_COUNT])
            .expect("opus");
        let AudioPayload::Opus(opus) = opus else {
            panic!("Opus esperado");
        };
        assert_eq!((opus.epoch(), opus.seq()), (2, 0));
        // Repetir el mismo perfil no abre época nueva.
        sender
            .set_choice(CodecChoice::Opus(CodecProfile::LowDelay5Ms))
            .expect("mismo perfil");
        let next = sender
            .encode(AudioFormat::Opus, &[0i16; SAMPLE_COUNT])
            .expect("opus 2");
        let AudioPayload::Opus(next) = next else {
            panic!("Opus esperado");
        };
        assert_eq!((next.epoch(), next.seq()), (2, 1));
    }
}
