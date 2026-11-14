//! Contrato binario del frame de audio v1 (28 B de cabecera + 480 B PCM16LE).

use arl_audio_core::frame::{
    AudioFrame, Flags, FrameError, CHANNELS, DATAGRAM_LEN, DURATION_US, HEADER_LEN,
    MAX_DATAGRAM_LEN, PAYLOAD_LEN, SAMPLE_COUNT, SAMPLE_RATE_HZ,
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
