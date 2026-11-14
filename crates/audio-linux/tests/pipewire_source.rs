//! Tests del camino real de la fuente PipeWire: SPSC, copia PCM16LE y callback.
//!
//! Todos los tests de copia ejercitan [`SampleConsumer::render_pcm16le_into`],
//! que es exactamente la función que invoca el callback de proceso del stream.
//! No hay una variante "de test" del render.

use arl_audio_core::pcm16::{from_le_bytes, BYTES_PER_SAMPLE, SILENCE};
use arl_audio_core::{PAYLOAD_LEN, SAMPLE_COUNT};
use arl_audio_linux::ring::{ring, SampleProducer};
use arl_audio_linux::{NegotiatedFormat, CHANNEL_COUNT, MEDIA_CLASS, NODE_NAME};

/// Frames que caben en un buffer de callback con el stride del contrato.
fn dst_for(frames: usize) -> Vec<u8> {
    vec![0u8; frames * BYTES_PER_SAMPLE]
}

/// Decodifica un frame completo del destino con el mismo wire del core.
fn frame_at(dst: &[u8], frame: usize) -> [i16; SAMPLE_COUNT] {
    let stride = BYTES_PER_SAMPLE;
    let start = frame * SAMPLE_COUNT * stride;
    let payload = &dst[start..start + PAYLOAD_LEN];
    from_le_bytes(payload).expect("payload de 480 bytes")
}

fn push_all(producer: &mut SampleProducer, samples: &[i16]) {
    let outcome = producer.push_slice(samples);
    assert_eq!(outcome.dropped, 0, "el test presupone capacidad suficiente");
    assert_eq!(outcome.written, samples.len());
}

#[test]
fn vacio_produce_silencio_sin_underrun_falso() {
    let (_producer, mut consumer) = ring(1024);
    let mut dst = dst_for(SAMPLE_COUNT);

    let outcome = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, SAMPLE_COUNT);

    assert_eq!(outcome.frames, SAMPLE_COUNT);
    assert_eq!(outcome.underrun_frames, SAMPLE_COUNT);
    assert_eq!(frame_at(&dst, 0), SILENCE);
    assert!(dst.iter().all(|&b| b == 0), "silencio es cero literal");
}

#[test]
fn fifo_conserva_orden_de_muestras() {
    let (mut producer, mut consumer) = ring(16);
    let samples: Vec<i16> = (1..=16).collect();
    push_all(&mut producer, &samples);

    let mut dst = dst_for(16);
    let outcome = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, 16);

    assert_eq!(outcome.frames, 16);
    assert_eq!(outcome.underrun_frames, 0);
    for i in 0..16 {
        let value = i16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
        assert_eq!(value, samples[i], "muestra {i} en orden");
    }
}

#[test]
fn overflow_descarta_el_excedente_y_no_crece() {
    let capacity = 8;
    let (mut producer, consumer) = ring(capacity);
    let capacity_before = producer.capacity();

    let outcome = producer.push_slice(&[7i16; 20]);

    assert_eq!(outcome.written, capacity, "acepta exactamente la capacidad");
    assert_eq!(outcome.dropped, 12, "el resto se descarta");
    assert_eq!(
        producer.capacity(),
        capacity_before,
        "la capacidad no crece"
    );
    assert_eq!(producer.dropped(), 12);
    assert_eq!(producer.free(), 0);
    assert_eq!(consumer.available(), capacity);
}

#[test]
fn wrap_around_preserva_continuidad_al_cruzar_el_borde() {
    let capacity = 32;
    let (mut producer, mut consumer) = ring(capacity);
    let mut dst = dst_for(16);
    let mut expected: i16 = 0;

    // Tres vueltas completas con tamaños que no dividen la capacidad: cada paso
    // deja el cursor en una posición distinta del anillo.
    for round in 0..3 {
        let take = 5 + round * 3;
        let push = 20 + round;
        let samples: Vec<i16> = (0..push).map(|i| expected + i as i16).collect();
        let outcome = producer.push_slice(&samples);
        assert_eq!(outcome.dropped, 0);

        let mut read = 0usize;
        while read < outcome.written {
            let chunk = (outcome.written - read).min(take);
            let rendered = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, chunk);
            assert_eq!(rendered.underrun_frames, 0, "sin huecos en el borde");
            for i in 0..chunk {
                let value = i16::from_le_bytes([dst[i * 2], dst[i * 2 + 1]]);
                assert_eq!(
                    value, expected,
                    "continuidad en vuelta {round}, offset {read}+{i}"
                );
                expected += 1;
            }
            read += chunk;
        }
    }
    assert!(producer.dropped() == 0, "ninguna vuelta descartó muestras");
}

#[test]
fn frame_parcial_no_se_pierde_ni_se_repite() {
    let (mut producer, mut consumer) = ring(2 * SAMPLE_COUNT);
    // Un frame del contrato empujado en tres trozos que no caen en el borde.
    let mut frame = [0i16; SAMPLE_COUNT];
    for (i, sample) in frame.iter_mut().enumerate() {
        *sample = (i as i16) - 120;
    }
    push_all(&mut producer, &frame[..97]);
    push_all(&mut producer, &frame[97..201]);
    push_all(&mut producer, &frame[201..]);

    let mut dst = dst_for(SAMPLE_COUNT);
    let outcome = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, SAMPLE_COUNT);

    assert_eq!(outcome.underrun_frames, 0);
    assert_eq!(
        frame_at(&dst, 0),
        frame,
        "muestras reconstruidas sin pérdida"
    );
}

#[test]
fn stride_mayor_que_2_rellena_sin_residuo() {
    let (mut producer, mut consumer) = ring(8);
    push_all(&mut producer, &[1234i16, -1234]);

    // Stride de 4 bytes (muestra + 2 bytes de relleno), como haría un
    // interleaved de 2 canales si alguien lo pidiera.
    let mut dst = vec![0xAAu8; 4 * 2];
    let outcome = consumer.render_pcm16le_into(&mut dst, 0, 4, 2);

    assert_eq!(outcome.frames, 2);
    assert_eq!(outcome.underrun_frames, 0);
    assert_eq!(&dst[0..2], &1234i16.to_le_bytes());
    assert_eq!(&dst[2..4], &[0, 0], "relleno limpio, no residuo 0xAA");
    assert_eq!(&dst[4..6], &(-1234i16).to_le_bytes());
    assert_eq!(&dst[6..8], &[0, 0]);
}

#[test]
fn destino_corto_acota_frames_y_no_escribe_fuera() {
    let (mut producer, mut consumer) = ring(1024);
    push_all(&mut producer, &[5i16; 600]);

    let mut dst = vec![0u8; 2 * 100]; // sólo caben 100 frames
    let outcome = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, 240);

    assert_eq!(outcome.frames, 100, "recortado a lo que cabe");
    assert_eq!(outcome.underrun_frames, 0);
    assert_eq!(consumer.available(), 500, "no consumió de más");
    assert_eq!(dst.len(), 200, "el destino conserva su longitud");
}

#[test]
fn offset_no_nulo_respeta_el_prefijo() {
    let (mut producer, mut consumer) = ring(8);
    push_all(&mut producer, &[9i16, 8]);

    let mut dst = vec![0x55u8; 8];
    let outcome = consumer.render_pcm16le_into(&mut dst, 4, BYTES_PER_SAMPLE, 2);

    assert_eq!(outcome.frames, 2);
    assert_eq!(&dst[0..4], &[0x55; 4], "el prefijo no se toca");
    assert_eq!(&dst[4..6], &9i16.to_le_bytes());
    assert_eq!(&dst[6..8], &8i16.to_le_bytes());
}

#[test]
fn stride_invalido_no_escribe_ni_consume() {
    let (mut producer, mut consumer) = ring(8);
    push_all(&mut producer, &[1i16, 2]);

    let mut dst = vec![0x11u8; 16];
    let outcome = consumer.render_pcm16le_into(&mut dst, 0, 3, 4);

    assert_eq!(outcome.frames, 0, "stride no alineado se rechaza");
    assert_eq!(outcome.underrun_frames, 0);
    assert_eq!(consumer.available(), 2, "no consumió muestras");
    assert!(dst.iter().all(|&b| b == 0x11), "no escribió nada");
}

#[test]
fn capacidad_cero_descarta_todo_sin_panico() {
    let (mut producer, mut consumer) = ring(0);
    let outcome = producer.push_slice(&[1i16; 4]);

    assert_eq!(outcome.written, 0);
    assert_eq!(outcome.dropped, 4);
    assert_eq!(producer.capacity(), 0);

    let mut dst = dst_for(SAMPLE_COUNT);
    let render = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, 2);
    assert_eq!(render.frames, 2);
    assert_eq!(render.underrun_frames, 2, "todo silencio, sin pánico");
    assert_eq!(frame_at(&dst, 0), SILENCE);
}

#[test]
fn underrun_no_deja_muestras_stale() {
    let (mut producer, mut consumer) = ring(8);
    push_all(&mut producer, &[1000i16, 1001]);

    // Prellena el destino con basura de un callback anterior.
    let mut dst: Vec<u8> = (0..8u8).map(|b| b.wrapping_mul(7)).collect();
    let outcome = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, 4);

    assert_eq!(outcome.frames, 4);
    assert_eq!(outcome.underrun_frames, 2);
    assert_eq!(&dst[0..2], &1000i16.to_le_bytes());
    assert_eq!(&dst[2..4], &1001i16.to_le_bytes());
    assert_eq!(&dst[4..6], &[0, 0], "underrun es silencio, no basura");
    assert_eq!(&dst[6..8], &[0, 0]);
}

#[test]
fn consumo_acumulado_reproduce_una_secuencia_larga() {
    let (mut producer, mut consumer) = ring(4096);
    let total = 240 * 6; // seis frames del contrato
    let samples: Vec<i16> = (0..total).map(|i| (i as i16).wrapping_mul(3)).collect();
    let mut pending = &samples[..];
    let mut dst = dst_for(SAMPLE_COUNT);
    let mut frame_index = 0;

    while !pending.is_empty() || frame_index < 6 {
        let outcome = producer.push_slice(pending);
        pending = &pending[outcome.written..];

        let rendered = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, SAMPLE_COUNT);
        assert_eq!(rendered.underrun_frames, 0, "frame {frame_index} completo");
        let decoded = frame_at(&dst, 0);
        let expected = &samples[frame_index * SAMPLE_COUNT..(frame_index + 1) * SAMPLE_COUNT];
        assert_eq!(decoded, expected, "frame {frame_index} intacto");
        frame_index += 1;
    }
    assert_eq!(frame_index, 6);
}

#[test]
fn constante_de_contrato_coincide_con_el_core() {
    assert_eq!(NODE_NAME, "arl-mic");
    assert_eq!(MEDIA_CLASS, "Audio/Source");
    assert_eq!(CHANNEL_COUNT, arl_audio_core::CHANNELS as u32);
    assert_eq!(BYTES_PER_SAMPLE, arl_audio_core::pcm16::BYTES_PER_SAMPLE);
    assert_eq!(PAYLOAD_LEN, SAMPLE_COUNT * 2);
}

#[test]
fn formato_negociado_se_valida_contra_el_contrato() {
    let exacto = NegotiatedFormat {
        rate: 48_000,
        channels: 1,
        s16le: true,
    };
    assert!(exacto.matches_contract());

    let estereo = NegotiatedFormat {
        channels: 2,
        ..exacto
    };
    assert!(!estereo.matches_contract());

    let float = NegotiatedFormat {
        s16le: false,
        ..exacto
    };
    assert!(!float.matches_contract());
    assert!(!NegotiatedFormat::UNSET.matches_contract());
}

/// El render no asigna memoria propia en su cuerpo (recorre `dst` ya reservado
/// y `pop`ea del anillo); este test sólo comprueba que la capacidad del anillo
/// no cambia entre llamadas. No mide el allocator ni el trampolín RT.
#[test]
fn render_repetido_no_altera_la_capacidad_del_anillo() {
    let (mut producer, mut consumer) = ring(64);
    let capacity = consumer.capacity();
    let mut dst = dst_for(SAMPLE_COUNT);

    for round in 0..1_000 {
        push_all(&mut producer, &[(round % 100) as i16; 32]);
        let outcome = consumer.render_pcm16le_into(&mut dst, 0, BYTES_PER_SAMPLE, 32);
        assert_eq!(outcome.frames, 32);
        assert_eq!(outcome.underrun_frames, 0);
        assert_eq!(consumer.capacity(), capacity);
        assert_eq!(producer.capacity(), capacity);
    }
}

/// Pasa el consumidor a una función que exige `&mut T`: compila para cualquier
/// tipo, incluido uno `Clone`. No prueba ausencia de `Clone` ni de `Send`; sólo
/// deja constancia de que la API de test trabaja sobre una instancia mutable
/// única del valor.
#[test]
fn consumidor_es_unico_y_no_clonable() {
    fn assert_unique<T>(_: &mut T) {}
    let (_producer, mut consumer) = ring(4);
    assert_unique(&mut consumer);
}
