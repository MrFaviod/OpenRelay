//! Contrato binario del frame de audio v1 (28 B de cabecera + 480 B PCM16LE).

use arl_audio_core::frame::{
    opus_duration_for, AudioFormat, AudioFrame, AudioPayload, Flags, FrameError, OpusFrame,
    CHANNELS, DATAGRAM_LEN, DURATION_US, FEATURE_AUDIO_OPUS, FEATURE_AUDIO_PCM16, HEADER_LEN,
    MAX_DATAGRAM_LEN, OPUS_MAX_PAYLOAD_LEN, PAYLOAD_LEN, SAMPLE_COUNT, SAMPLE_RATE_HZ, TYPE_AUDIO,
    TYPE_AUDIO_OPUS,
};
use arl_audio_core::pcm16::{self, Samples};

fn samples() -> Samples {
    let mut out = [0i16; SAMPLE_COUNT];
    out[0] = 0x1234;
    out[1] = -2;
    out[SAMPLE_COUNT - 1] = i16::MIN;
    out
}

fn golden() -> [u8; DATAGRAM_LEN] {
    AudioFrame::new(
        0x0102_0304,
        0x0506_0708,
        Flags::SILENCE | Flags::DISCONTINUITY,
        samples(),
    )
    .expect("frame válido")
    .encode()
}

fn put_u16(buf: &mut [u8; DATAGRAM_LEN], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_u32(buf: &mut [u8; DATAGRAM_LEN], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

#[test]
fn layout_exacto_508_bytes() {
    let buf = golden();
    assert_eq!(DATAGRAM_LEN, 508);
    assert_eq!(HEADER_LEN, 28);
    assert_eq!(PAYLOAD_LEN, 480);
    assert_eq!(MAX_DATAGRAM_LEN, 1200);
    assert_eq!(SAMPLE_COUNT, 240);
    assert_eq!(SAMPLE_RATE_HZ, 48_000);
    assert_eq!(CHANNELS, 1);
    assert_eq!(DURATION_US, 5_000);
    assert_eq!(buf.len(), 508);

    assert_eq!(buf[0], 1, "type audio");
    assert_eq!(buf[1], 1, "version 1");
    assert_eq!(buf[2], 0b11, "flags silence|discontinuity");
    assert_eq!(buf[3], 28, "header_len");
    assert_eq!(&buf[4..8], &[1, 2, 3, 4], "epoch BE");
    assert_eq!(&buf[8..12], &[5, 6, 7, 8], "seq BE");
    assert_eq!(&buf[12..14], &240u16.to_be_bytes(), "sample_count BE");
    assert_eq!(&buf[14..16], &5_000u16.to_be_bytes(), "duration_us BE");
    assert_eq!(&buf[16..20], &48_000u32.to_be_bytes(), "sample_rate BE");
    assert_eq!(&buf[20..22], &1u16.to_be_bytes(), "channels BE");
    assert_eq!(&buf[22..24], &[0, 0], "reserved");
    assert_eq!(&buf[24..28], &480u32.to_be_bytes(), "payload_len BE");
    assert_eq!(&buf[28..30], &[0x34, 0x12], "PCM16LE primer sample");
    assert_eq!(&buf[30..32], &[0xfe, 0xff], "PCM16LE -2");
    assert_eq!(&buf[506..508], &[0x00, 0x80], "PCM16LE i16::MIN");
}

#[test]
fn roundtrip_conserva_campos_y_muestras() {
    let frame = AudioFrame::new(7, 1_000, Flags::DISCONTINUITY, samples()).expect("válido");
    let decoded = AudioFrame::decode(&frame.encode()).expect("decodifica");
    assert_eq!(decoded.epoch(), 7);
    assert_eq!(decoded.seq(), 1_000);
    assert_eq!(decoded.flags(), Flags::DISCONTINUITY);
    assert_eq!(decoded.samples(), &samples());
    assert_eq!(decoded, frame);
}

#[test]
fn acepta_limites_de_epoch_seq_y_flags() {
    for epoch in [1, u32::MAX] {
        for seq in [0, u32::MAX] {
            for flags in 0..=3u8 {
                let flags = Flags::new(flags).expect("flags válidos");
                let frame = AudioFrame::new(epoch, seq, flags, pcm16::SILENCE).expect("válido");
                let decoded = AudioFrame::decode(&frame.encode()).expect("decodifica");
                assert_eq!(
                    (decoded.epoch(), decoded.seq(), decoded.flags()),
                    (epoch, seq, flags)
                );
            }
        }
    }
}

#[test]
fn rechaza_type_version_header_len_y_flags_invalidos() {
    let mut buf = golden();
    buf[0] = 2;
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidType(2))
    ));

    let mut buf = golden();
    buf[1] = 2;
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidVersion(2))
    ));

    let mut buf = golden();
    buf[3] = 27;
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidHeaderLen(27))
    ));

    for bits in [0x04u8, 0x08, 0x80, 0xff] {
        assert!(Flags::new(bits).is_err());
        let mut buf = golden();
        buf[2] = bits;
        assert!(matches!(
            AudioFrame::decode(&buf),
            Err(FrameError::InvalidFlags { .. })
        ));
    }
}

#[test]
fn rechaza_epoch_cero_y_constantes_de_cabecera() {
    let mut buf = golden();
    put_u32(&mut buf, 4, 0);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidEpoch)
    ));

    let mut buf = golden();
    put_u16(&mut buf, 12, 239);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidSampleCount(239))
    ));

    let mut buf = golden();
    put_u16(&mut buf, 14, 4_999);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidDuration(4_999))
    ));

    let mut buf = golden();
    put_u32(&mut buf, 16, 44_100);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidSampleRate(44_100))
    ));

    let mut buf = golden();
    put_u16(&mut buf, 20, 2);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidChannels(2))
    ));

    let mut buf = golden();
    put_u16(&mut buf, 22, 1);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidReserved(1))
    ));

    let mut buf = golden();
    put_u32(&mut buf, 24, 479);
    assert!(matches!(
        AudioFrame::decode(&buf),
        Err(FrameError::InvalidPayloadLen(479))
    ));
}

#[test]
fn rechaza_tamanos_que_no_son_un_frame_exacto() {
    let buf = golden();
    assert!(matches!(
        AudioFrame::decode(&buf[..27]),
        Err(FrameError::TooShort { len: 27 })
    ));
    assert!(matches!(
        AudioFrame::decode(&buf[..507]),
        Err(FrameError::LengthMismatch {
            expected: 508,
            actual: 507
        })
    ));
    let bigger = [0u8; 509];
    assert!(matches!(
        AudioFrame::decode(&bigger),
        Err(FrameError::LengthMismatch {
            expected: 508,
            actual: 509
        })
    ));
    let oversized = vec![0u8; MAX_DATAGRAM_LEN + 1];
    assert!(matches!(
        AudioFrame::decode(&oversized),
        Err(FrameError::Oversized { len: 1201 })
    ));
}

#[test]
fn constructor_rechaza_epoch_cero() {
    assert!(matches!(
        AudioFrame::new(0, 0, Flags::NONE, pcm16::SILENCE),
        Err(FrameError::InvalidEpoch)
    ));
    assert!(AudioFrame::new(1, 0, Flags::NONE, pcm16::SILENCE).is_ok());
}

#[test]
fn pcm16_es_little_endian_y_exige_480_bytes() {
    let mut samples = pcm16::SILENCE;
    samples[0] = 0x1234;
    samples[1] = -2;
    samples[2] = i16::MAX;
    let bytes = pcm16::to_le_bytes(&samples);
    assert_eq!(&bytes[0..2], &[0x34, 0x12]);
    assert_eq!(&bytes[2..4], &[0xfe, 0xff]);
    assert_eq!(&bytes[4..6], &[0xff, 0x7f]);
    assert_eq!(pcm16::from_le_bytes(&bytes).expect("480 B"), samples);
    assert!(matches!(
        pcm16::from_le_bytes(&bytes[..479]),
        Err(FrameError::PayloadLength {
            expected: 480,
            actual: 479
        })
    ));
}

#[test]
fn encode_into_no_asigna_y_es_identico_a_encode() {
    let frame = AudioFrame::new(1, 0, Flags::NONE, samples()).expect("válido");
    let mut out = [0u8; DATAGRAM_LEN];
    frame.encode_into(&mut out);
    assert_eq!(out, frame.encode());
}

#[test]
fn error_tiene_mensaje_legible() {
    let err = AudioFrame::decode(&[0u8; 0]).expect_err("vacío inválido");
    assert!(matches!(err, FrameError::TooShort { len: 0 }));
    assert!(!err.to_string().is_empty());
    assert!(std::error::Error::source(&err).is_none());
}

fn opus_packet(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn put_at_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_at_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

#[test]
fn opus_roundtrip_en_las_tres_duraciones() {
    for &(sample_count, duration_us) in &[(120u16, 2_500u16), (240, 5_000), (480, 10_000)] {
        let packet = opus_packet(97);
        let frame = OpusFrame::new(9, 42, Flags::DISCONTINUITY, sample_count, &packet)
            .expect("frame Opus válido");
        let (wire, len) = frame.encode();
        assert_eq!(len, HEADER_LEN + packet.len());
        assert_eq!(wire[0], TYPE_AUDIO_OPUS);
        assert_eq!(wire[1], 1);
        assert_eq!(wire[2], Flags::DISCONTINUITY.bits());
        assert_eq!(wire[3], 28);
        assert_eq!(&wire[4..8], &9u32.to_be_bytes());
        assert_eq!(&wire[8..12], &42u32.to_be_bytes());
        assert_eq!(&wire[12..14], &sample_count.to_be_bytes());
        assert_eq!(&wire[14..16], &duration_us.to_be_bytes());
        assert_eq!(&wire[16..20], &48_000u32.to_be_bytes());
        assert_eq!(&wire[20..22], &1u16.to_be_bytes());
        assert_eq!(&wire[22..24], &[0, 0]);
        assert_eq!(&wire[24..28], &(packet.len() as u32).to_be_bytes());
        assert_eq!(&wire[28..len], packet.as_slice());

        let decoded = OpusFrame::decode(&wire[..len]).expect("decodifica");
        assert_eq!(decoded, frame);
        assert_eq!(decoded.sample_count(), sample_count);
        assert_eq!(decoded.duration_us(), duration_us);
        assert_eq!(decoded.packet(), packet.as_slice());
    }
}

#[test]
fn opus_acepta_el_payload_maximo() {
    let packet = opus_packet(OPUS_MAX_PAYLOAD_LEN);
    let frame = OpusFrame::new(1, 0, Flags::NONE, 240, &packet).expect("1172 B válidos");
    let (wire, len) = frame.encode();
    assert_eq!(len, MAX_DATAGRAM_LEN);
    assert_eq!(OpusFrame::decode(&wire[..len]).expect("máximo"), frame);
}

#[test]
fn opus_constructor_rechaza_payload_vacio_u_oversize() {
    assert!(matches!(
        OpusFrame::new(1, 0, Flags::NONE, 240, &[]),
        Err(FrameError::InvalidOpusPayloadLen(0))
    ));
    let too_big = opus_packet(OPUS_MAX_PAYLOAD_LEN + 1);
    assert!(matches!(
        OpusFrame::new(1, 0, Flags::NONE, 240, &too_big),
        Err(FrameError::InvalidOpusPayloadLen(1173))
    ));
}

#[test]
fn opus_rechaza_sample_count_y_duracion_invalidos() {
    let packet = opus_packet(60);
    assert!(matches!(
        OpusFrame::new(1, 0, Flags::NONE, 160, &packet),
        Err(FrameError::InvalidOpusSampleCount(160))
    ));
    assert_eq!(opus_duration_for(120), Some(2_500));
    assert_eq!(opus_duration_for(240), Some(5_000));
    assert_eq!(opus_duration_for(480), Some(10_000));
    assert_eq!(opus_duration_for(0), None);
    assert_eq!(opus_duration_for(360), None);

    let (wire, len) = OpusFrame::new(1, 0, Flags::NONE, 240, &packet)
        .expect("válido")
        .encode();
    let mut bad = wire;
    put_at_u16(&mut bad, 12, 160);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidOpusSampleCount(160))
    ));

    let mut bad = wire;
    put_at_u16(&mut bad, 14, 4_999);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidOpusDuration(4_999))
    ));
}

#[test]
fn opus_rechaza_cabecera_y_longitudes_incoherentes() {
    let (wire, len) = OpusFrame::new(1, 7, Flags::NONE, 240, &opus_packet(60))
        .expect("válido")
        .encode();

    let mut bad = wire;
    bad[0] = TYPE_AUDIO;
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidType(1))
    ));

    let mut bad = wire;
    bad[1] = 2;
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidVersion(2))
    ));

    let mut bad = wire;
    bad[2] = 0x04;
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidFlags { .. })
    ));

    let mut bad = wire;
    bad[3] = 27;
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidHeaderLen(27))
    ));

    let mut bad = wire;
    put_at_u32(&mut bad, 4, 0);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidEpoch)
    ));

    let mut bad = wire;
    put_at_u32(&mut bad, 16, 44_100);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidSampleRate(44_100))
    ));

    let mut bad = wire;
    put_at_u16(&mut bad, 20, 2);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidChannels(2))
    ));

    let mut bad = wire;
    put_at_u16(&mut bad, 22, 1);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidReserved(1))
    ));

    let mut bad = wire;
    put_at_u32(&mut bad, 24, 0);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidOpusPayloadLen(0))
    ));

    let mut bad = wire;
    put_at_u32(&mut bad, 24, 1_173);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::InvalidOpusPayloadLen(1_173))
    ));

    let mut bad = wire;
    put_at_u32(&mut bad, 24, 61);
    assert!(matches!(
        OpusFrame::decode(&bad[..len]),
        Err(FrameError::PayloadLenMismatch {
            declared: 61,
            actual: 60
        })
    ));

    assert!(matches!(
        OpusFrame::decode(&wire[..len - 1]),
        Err(FrameError::PayloadLenMismatch {
            declared: 60,
            actual: 59
        })
    ));
    assert!(matches!(
        OpusFrame::decode(&wire[..HEADER_LEN - 1]),
        Err(FrameError::TooShort { len: 27 })
    ));
    let oversized = vec![0u8; MAX_DATAGRAM_LEN + 1];
    assert!(matches!(
        OpusFrame::decode(&oversized),
        Err(FrameError::Oversized { len: 1_201 })
    ));
}

#[test]
fn audio_payload_despacha_por_type_y_pcm16_sigue_intacto() {
    let pcm = golden();
    assert_eq!(pcm[0], TYPE_AUDIO);
    assert_eq!(pcm.len(), DATAGRAM_LEN);
    match AudioPayload::decode(&pcm).expect("pcm válido") {
        AudioPayload::Pcm16(frame) => {
            assert_eq!(frame.encode(), pcm);
            assert_eq!(frame.samples(), &samples());
        }
        other => panic!("se esperaba PCM16, llegó {other:?}"),
    }
    let (pcm_wire, pcm_len) = AudioPayload::Pcm16(
        AudioFrame::new(
            0x0102_0304,
            0x0506_0708,
            Flags::SILENCE | Flags::DISCONTINUITY,
            samples(),
        )
        .expect("válido"),
    )
    .encode();
    assert_eq!(pcm_len, DATAGRAM_LEN);
    assert_eq!((&pcm_wire[..pcm_len]), pcm.as_slice());

    let opus = OpusFrame::new(1, 0, Flags::NONE, 120, &opus_packet(3)).expect("opus válido");
    let (wire, len) = opus.encode();
    match AudioPayload::decode(&wire[..len]).expect("opus válido") {
        AudioPayload::Opus(frame) => {
            assert_eq!(frame, opus);
            assert_eq!(TYPE_AUDIO_OPUS, wire[0]);
        }
        other => panic!("se esperaba Opus, llegó {other:?}"),
    }
    let (opus_wire, opus_len) = AudioPayload::Opus(opus).encode();
    assert_eq!(opus_len, HEADER_LEN + 3);
    assert_eq!((&opus_wire[..opus_len]), (&wire[..len]));

    let mut unknown = pcm;
    unknown[0] = 9;
    assert!(matches!(
        AudioPayload::decode(&unknown),
        Err(FrameError::InvalidType(9))
    ));
    assert!(matches!(
        AudioPayload::decode(&[]),
        Err(FrameError::TooShort { len: 0 })
    ));
}

#[test]
fn opus_solo_se_usa_si_ambos_extremos_lo_anuncian() {
    let ambos = [FEATURE_AUDIO_PCM16, FEATURE_AUDIO_OPUS];
    let solo_pcm = [FEATURE_AUDIO_PCM16, "session.v1"];
    assert_eq!(AudioFormat::negotiate(&ambos, &ambos), AudioFormat::Opus);
    assert_eq!(
        AudioFormat::negotiate(&ambos, &solo_pcm),
        AudioFormat::Pcm16
    );
    assert_eq!(
        AudioFormat::negotiate(&solo_pcm, &ambos),
        AudioFormat::Pcm16
    );
    assert_eq!(
        AudioFormat::negotiate(&solo_pcm, &solo_pcm),
        AudioFormat::Pcm16
    );
    assert_eq!(AudioFormat::negotiate(&[], &ambos), AudioFormat::Pcm16);
}
