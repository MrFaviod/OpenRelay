//! Pruebas de los perfiles Opus restricted-lowdelay de `openrelay-codec`.

use openrelay_codec::{
    CodecError, CodecProfile, OpusDecoder, OpusEncoder, FRAME_HEADER_LEN, LOOKAHEAD_US,
    MAX_BITRATE_BPS, MAX_DATAGRAM_LEN, MAX_PACKET_LEN, MIN_BITRATE_BPS, SAMPLE_RATE_HZ,
};

const BITRATES: [u32; 3] = [64_000, 80_000, 96_000];

/// Seno de 440 Hz con amplitud moderada, sin fundido, para medir SNR.
fn sine(samples: usize, phase: f64) -> Vec<i16> {
    const FREQ_HZ: f64 = 440.0;
    const AMPLITUDE: f64 = 12_000.0;
    (0..samples)
        .map(|n| {
            let t = (n as f64) / f64::from(SAMPLE_RATE_HZ);
            (AMPLITUDE * (2.0 * std::f64::consts::PI * FREQ_HZ * t + phase).sin()).round() as i16
        })
        .collect()
}

/// Energía media por muestra.
fn energy(pcm: &[i16]) -> f64 {
    if pcm.is_empty() {
        return 0.0;
    }
    pcm.iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / pcm.len() as f64
}

/// SNR en dB entre la entrada y la salida decodificada, compensando el
/// retardo del códec con una búsqueda acotada de desfase.
fn snr_db(reference: &[i16], decoded: &[i16], max_lag: usize) -> f64 {
    let mut best = (f64::NEG_INFINITY, 0usize);
    for lag in 0..=max_lag.min(decoded.len().saturating_sub(1)) {
        let available = decoded.len() - lag;
        let skip = 480.min(available / 4);
        if available <= skip + 480 {
            continue;
        }
        let mut signal = 0.0;
        let mut noise = 0.0;
        for i in skip..(available - skip) {
            let reference_sample = f64::from(reference[i]);
            let decoded_sample = f64::from(decoded[i + lag]);
            signal += reference_sample * reference_sample;
            noise += (decoded_sample - reference_sample).powi(2);
        }
        let ratio = if noise <= f64::EPSILON {
            f64::INFINITY
        } else {
            10.0 * (signal / noise).log10()
        };
        if ratio > best.0 {
            best = (ratio, lag);
        }
    }
    best.0
}

#[test]
fn profiles_have_expected_sample_counts_and_durations() {
    let expected = [
        (CodecProfile::LowDelay2p5Ms, 120, 2_500),
        (CodecProfile::LowDelay5Ms, 240, 5_000),
        (CodecProfile::LowDelay10Ms, 480, 10_000),
    ];
    assert_eq!(CodecProfile::ALL.len(), expected.len());
    for (profile, samples, duration_us) in expected {
        assert_eq!(profile.samples_per_frame(), samples);
        assert_eq!(profile.frame_duration_us(), duration_us);
        assert_eq!(
            CodecProfile::from_frame_samples(samples).expect("perfil válido"),
            profile
        );
    }
}

#[test]
fn roundtrip_all_frame_sizes_and_bitrate_bounds() {
    for profile in CodecProfile::ALL {
        for bitrate_bps in BITRATES {
            let mut encoder = OpusEncoder::new(profile, bitrate_bps).expect("encoder");
            let mut decoder = OpusDecoder::new(profile).expect("decoder");
            let mut phase = 0.0;
            let mut decoded = Vec::new();
            let mut reference = Vec::new();
            for _ in 0..10 {
                let pcm = sine(profile.samples_per_frame(), phase);
                phase += 0.01;
                let packet = encoder.encode(&pcm).expect("encode");
                assert!(!packet.is_empty(), "paquete vacío");
                let frame = decoder.decode(Some(&packet)).expect("decode");
                assert_eq!(frame.len(), profile.samples_per_frame());
                reference.extend_from_slice(&pcm);
                decoded.extend_from_slice(&frame);
            }
            let snr = snr_db(&reference, &decoded, 480);
            let floor_db = if profile == CodecProfile::LowDelay2p5Ms {
                10.0
            } else {
                15.0
            };
            assert!(
                snr > floor_db,
                "{profile:?} a {bitrate_bps} bps: SNR {snr:.1} dB <= {floor_db} dB"
            );
            let ratio = energy(&decoded) / energy(&reference);
            assert!(
                (0.5..=2.0).contains(&ratio),
                "{profile:?} a {bitrate_bps} bps: energía relativa {ratio:.2}"
            );
        }
    }
}

#[test]
fn rejects_invalid_frame_sizes_and_pcm_lengths() {
    for samples in [0, 1, 119, 121, 239, 241, 479, 481, 960] {
        assert!(matches!(
            CodecProfile::from_frame_samples(samples),
            Err(CodecError::InvalidFrameSamples { samples: actual }) if actual == samples
        ));
    }

    for profile in CodecProfile::ALL {
        let mut encoder = OpusEncoder::new(profile, MIN_BITRATE_BPS).expect("encoder");
        let wrong = vec![0i16; profile.samples_per_frame() + 1];
        assert!(matches!(
            encoder.encode(&wrong),
            Err(CodecError::InvalidPcmLength { .. })
        ));
        let empty: Vec<i16> = Vec::new();
        assert!(matches!(
            encoder.encode(&empty),
            Err(CodecError::InvalidPcmLength { .. })
        ));
    }
}

#[test]
fn rejects_invalid_bitrates_and_accepts_bounds() {
    for bitrate_bps in [0, 32_000, MIN_BITRATE_BPS - 1, MAX_BITRATE_BPS + 1, 128_000] {
        assert!(matches!(
            OpusEncoder::new(CodecProfile::LowDelay5Ms, bitrate_bps),
            Err(CodecError::InvalidBitrate { bitrate_bps: actual }) if actual == bitrate_bps
        ));
    }
    for bitrate_bps in [MIN_BITRATE_BPS, MAX_BITRATE_BPS] {
        let encoder =
            OpusEncoder::new(CodecProfile::LowDelay5Ms, bitrate_bps).expect("bitrate aceptado");
        assert_eq!(encoder.bitrate_bps(), bitrate_bps);
    }
}

#[test]
fn plc_produces_correct_length_output() {
    for profile in CodecProfile::ALL {
        let mut encoder = OpusEncoder::new(profile, MAX_BITRATE_BPS).expect("encoder");
        let mut decoder = OpusDecoder::new(profile).expect("decoder");
        let samples = profile.samples_per_frame();

        let mut last = Vec::new();
        for _ in 0..8 {
            let pcm = sine(samples, 0.0);
            let packet = encoder.encode(&pcm).expect("encode");
            last = decoder.decode(Some(&packet)).expect("decode");
        }
        assert_eq!(last.len(), samples);

        // `None` y un paquete vacío son la misma señal de pérdida para libopus.
        for concealed in [
            decoder.decode(None).expect("PLC con None"),
            decoder.decode(Some(&[])).expect("PLC con paquete vacío"),
        ] {
            assert_eq!(concealed.len(), samples);
            assert!(
                concealed.iter().any(|sample| *sample != 0),
                "{profile:?}: PLC devolvió silencio absoluto"
            );
        }

        // Un paquete por encima del presupuesto se rechaza antes de libopus.
        let oversized = vec![0u8; MAX_PACKET_LEN + 1];
        assert!(matches!(
            decoder.decode(Some(&oversized)),
            Err(CodecError::PacketTooLarge { .. })
        ));
    }
}

#[test]
fn packets_fit_datagram_budget() {
    assert_eq!(MAX_PACKET_LEN, 1200 - 28);
    assert_eq!(MAX_PACKET_LEN, MAX_DATAGRAM_LEN - FRAME_HEADER_LEN);

    for profile in CodecProfile::ALL {
        for bitrate_bps in BITRATES {
            let mut encoder = OpusEncoder::new(profile, bitrate_bps).expect("encoder");
            for _ in 0..20 {
                let pcm = sine(profile.samples_per_frame(), 0.0);
                let packet = encoder.encode(&pcm).expect("encode");
                assert!(
                    packet.len() <= MAX_PACKET_LEN,
                    "{profile:?} a {bitrate_bps} bps: paquete de {} bytes",
                    packet.len()
                );
            }
        }
    }
}

#[test]
fn snr_sanity_vs_fixed_pcm16_on_sine() {
    // El PCM16 fijo ocupa 480 bytes por trama de 5 ms (768 kbps).
    // Comparamos la señal decodificada contra la entrada original: debe
    // conservar la forma de onda, no sólo la energía.
    let profile = CodecProfile::LowDelay5Ms;
    let mut encoder = OpusEncoder::new(profile, 96_000).expect("encoder");
    let mut decoder = OpusDecoder::new(profile).expect("decoder");
    let mut reference = Vec::new();
    let mut decoded = Vec::new();
    for _ in 0..20 {
        let pcm = sine(profile.samples_per_frame(), 0.0);
        let packet = encoder.encode(&pcm).expect("encode");
        assert!(packet.len() <= 480, "Opus debería batir al PCM16 fijo");
        let frame = decoder.decode(Some(&packet)).expect("decode");
        reference.extend_from_slice(&pcm);
        decoded.extend_from_slice(&frame);
    }
    let snr = snr_db(&reference, &decoded, 480);
    assert!(snr > 20.0, "SNR {snr:.1} dB demasiado baja en un seno");
}

#[test]
fn lookahead_is_2_5_ms() {
    let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
    let lookahead_samples = encoder.lookahead_samples().expect("lookahead");
    assert_eq!(lookahead_samples, 120);
    assert_eq!(
        u64::from(lookahead_samples) * 1_000_000 / u64::from(SAMPLE_RATE_HZ),
        u64::from(LOOKAHEAD_US)
    );
}
