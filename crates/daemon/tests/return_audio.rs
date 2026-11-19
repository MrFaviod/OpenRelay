//! Tests del retorno Linux→Android (F8/T24).
//!
//! Sin la feature `return-audio` este archivo compila vacío y el binario de
//! test termina con 0: la puerta es el `cfg` a nivel de archivo, no un
//! `#[ignore]`.

#![cfg(feature = "return-audio")]

use arl_audio_core::{AudioFrame, SAMPLE_COUNT, TYPE_AUDIO};
use arl_daemon::return_audio::{
    effective_local_features, should_send, PcmSource, ReturnEmitter, RETURN_EPOCH,
};
use arl_proto::control::Feature;

/// Fuente de prueba con bloque de lectura acotado, para forzar el ensamblado
/// de un frame a partir de varias lecturas parciales.
struct FakeSource {
    data: Vec<i16>,
    pos: usize,
    chunk: usize,
    reads: usize,
}

impl FakeSource {
    fn new(blocks: usize) -> Self {
        let data = (0..blocks * SAMPLE_COUNT)
            .map(|index| index as i16)
            .collect::<Vec<_>>();
        Self {
            data,
            pos: 0,
            chunk: 100,
            reads: 0,
        }
    }
}

impl PcmSource for FakeSource {
    fn read(&mut self, out: &mut [i16]) -> usize {
        self.reads += 1;
        let available = self.data.len() - self.pos;
        let count = self.chunk.min(out.len()).min(available);
        out[..count].copy_from_slice(&self.data[self.pos..self.pos + count]);
        self.pos += count;
        count
    }
}

/// Sólo se emite con las dos features y la clave local encendida.
#[test]
fn should_send_exige_ambas_features_y_config() {
    let peer_completo = [Feature::AudioPcm16_48kMono, Feature::AudioReturnPcm48kMono];
    let peer_sin_retorno = [Feature::AudioPcm16_48kMono];
    let local_completo = effective_local_features(true);
    let local_sin_retorno = effective_local_features(false);

    assert!(should_send(&peer_completo, &local_completo, true));
    // Peer sin la feature de retorno.
    assert!(!should_send(&peer_sin_retorno, &local_completo, true));
    // Extremo local sin la feature de retorno.
    assert!(!should_send(&peer_completo, &local_sin_retorno, true));
    // Ninguno de los dos la anuncia.
    assert!(!should_send(&peer_sin_retorno, &local_sin_retorno, true));
    // Config local apagada: ni con las dos features se emite.
    assert!(!should_send(&peer_completo, &local_completo, false));
}

/// Las features locales efectivas dependen de la config: sin
/// `audio.return_enabled` el daemon no anuncia el retorno.
#[test]
fn local_efectivo_no_anuncia_retorno_si_la_config_esta_apagada() {
    let apagado = effective_local_features(false);
    assert!(apagado.contains(&Feature::AudioPcm16_48kMono));
    assert!(!apagado.contains(&Feature::AudioReturnPcm48kMono));

    let encendido = effective_local_features(true);
    assert!(encendido.contains(&Feature::AudioPcm16_48kMono));
    assert!(encendido.contains(&Feature::AudioReturnPcm48kMono));
}

/// El emisor arma frames `type=1` decodificables con secuencia creciente,
/// aunque la fuente entregue bloques parciales.
#[test]
fn el_emisor_arma_frames_type_1_con_secuencia_creciente() {
    let mut emitter =
        ReturnEmitter::new(FakeSource::new(2), RETURN_EPOCH, true).expect("época distinta de cero");

    let first = emitter.pull().expect("primer frame");
    let second = emitter.pull().expect("segundo frame");

    assert_eq!(first.header().epoch(), RETURN_EPOCH);
    assert_eq!(first.seq(), 0);
    assert_eq!(second.seq(), 1);
    assert_eq!(first.samples()[0], 0);
    assert_eq!(first.samples()[SAMPLE_COUNT - 1], (SAMPLE_COUNT - 1) as i16);
    assert_eq!(second.samples()[0], SAMPLE_COUNT as i16);

    let wire = first.encode();
    assert_eq!(wire[0], TYPE_AUDIO);
    let decoded = AudioFrame::decode(&wire).expect("frame v1 decodificable");
    assert_eq!(decoded, first);

    // Fuente agotada y sin bloque parcial: no inventa un tercer frame.
    assert!(emitter.pull().is_none());
    assert_eq!(emitter.next_seq(), 2);
}

/// Un envío fallido no consume la secuencia: `peek` no avanza y sólo `commit`
/// la mueve, así que el próximo frame reusa el número sin hueco falso.
#[test]
fn un_envio_fallido_no_consume_la_secuencia() {
    let mut emitter =
        ReturnEmitter::new(FakeSource::new(3), RETURN_EPOCH, true).expect("época distinta de cero");

    let first = emitter.peek().expect("primer frame");
    assert_eq!(first.seq(), 0);
    assert_eq!(emitter.next_seq(), 0, "peek no consume la secuencia");
    // Fallo simulado: el frame se descarta sin commit y no se reenvía.
    let retry = emitter.peek().expect("mismo número para el próximo envío");
    assert_eq!(retry.seq(), 0);
    emitter.commit();
    assert_eq!(emitter.next_seq(), 1);
    assert_eq!(emitter.peek().expect("segundo frame").seq(), 1);
}

/// Con `should_send` en falso no sale ni un frame y la fuente ni se toca.
#[test]
fn sin_should_send_no_se_emite_nada() {
    let peer_sin_retorno = [Feature::AudioPcm16_48kMono];
    let enabled = should_send(&peer_sin_retorno, &effective_local_features(true), true);
    assert!(!enabled);

    let mut emitter = ReturnEmitter::new(FakeSource::new(1), RETURN_EPOCH, enabled)
        .expect("época distinta de cero");
    for _ in 0..8 {
        assert!(emitter.pull().is_none());
    }
    assert_eq!(emitter.source().reads, 0, "no se lee la fuente apagada");
    assert_eq!(emitter.next_seq(), 0, "la secuencia no avanza");
}
