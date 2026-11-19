//! Negociación Opus de extremo a extremo: el cliente anuncia
//! `audio.opus-48k-mono` en `capabilities`, el daemon lo registra y sólo
//! entonces acepta y decodifica frames `type=2`. Sin anuncio, los descarta.

#[path = "common/mod.rs"]
mod common;

use common::{exchange, session_setup, TestCa, TestLeaf, CLIENT_ADDR, SERVER_ADDR};
use openrelay_audio_core::{AudioPayload, Flags, OpusFrame};
use openrelay_codec::{CodecProfile, OpusEncoder};
use openrelay_daemon::control::{ControlRegistry, IdentityGrant, SharedControl};
use openrelay_daemon::session::SessionAudio;
use openrelay_proto::control::{
    Capabilities as ControlCapabilities, Feature, PeerRole, RemoteAction,
};
use openrelay_proto::wire::{CapabilitiesMsg, ControlMessage, HelloMsg, SCHEMA_VERSION};
use openrelay_transport::quinn::{ServerEvent, SessionId};

const ANDROID_DEVICE: &str = "android_test";

/// Cliente autorizado y sesión de control compartida listos para hablar.
fn setup() -> (
    openrelay_transport::quinn::SessionClient,
    openrelay_transport::quinn::SessionServer,
    SessionId,
    SharedControl,
    openrelay_transport::identity::SpkiPin,
) {
    let ca = TestCa::new();
    let server_leaf: TestLeaf = ca.leaf("openrelay-opus-server", true, false);
    let client_leaf: TestLeaf = ca.leaf("openrelay-opus-client", false, false);
    let (mut client, mut server) =
        session_setup(&client_leaf, &server_leaf, &ca, Some(&client_leaf)).expect("sesión");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    assert!(client.is_authorized(), "cliente autorizado");

    let session = std::iter::from_fn(|| server.next_event())
        .find_map(|event| match event {
            ServerEvent::Authorized { session, .. } => Some(session),
            _ => None,
        })
        .expect("sesión autorizada");

    let mut registry = ControlRegistry::new();
    registry
        .register(
            client_leaf.pin,
            IdentityGrant {
                device_id: ANDROID_DEVICE.into(),
                role: PeerRole::Android,
                actions: vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
            },
        )
        .expect("registro");
    let control = SharedControl::new(registry);
    (client, server, session, control, client_leaf.pin)
}

fn hello() -> ControlMessage {
    ControlMessage::Hello(HelloMsg {
        request_id: "opus-hello".into(),
        role: PeerRole::Android,
        version: "0.1.0".into(),
        protocol_version: SCHEMA_VERSION.into(),
    })
}

fn capabilities(advertise_opus: bool) -> ControlMessage {
    let mut capabilities = ControlCapabilities::from_peer(
        ANDROID_DEVICE,
        PeerRole::Android,
        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
    );
    capabilities.features = if advertise_opus {
        vec![
            Feature::AudioPcm16_48kMono,
            Feature::AudioOpus48kMono,
            Feature::ControlV1,
        ]
    } else {
        vec![Feature::AudioPcm16_48kMono, Feature::ControlV1]
    };
    ControlMessage::Capabilities(CapabilitiesMsg {
        request_id: "opus-caps".into(),
        revision: 0,
        capabilities,
    })
}

/// Un frame Opus real, codificado con el perfil de 5 ms.
fn opus_payload() -> AudioPayload {
    let mut encoder = OpusEncoder::new(CodecProfile::LowDelay5Ms, 64_000).expect("encoder");
    let mut pcm = [0i16; 240];
    for (index, sample) in pcm.iter_mut().enumerate() {
        *sample = ((index as i32 * 37) % 2000 - 1000) as i16;
    }
    let packet = encoder.encode(&pcm).expect("encode");
    AudioPayload::Opus(OpusFrame::new(1, 0, Flags::NONE, 240, &packet).expect("frame opus"))
}

/// `announced = true`: el daemon acepta y decodifica. `false`: descarta el
/// mismo frame, aunque el transporte lo haya entregado.
fn run(announced: bool) -> (SessionAudio, u64) {
    let (mut client, mut server, session, mut control, pin) = setup();
    assert!(
        !control.opus_negotiated(&pin),
        "sin capabilities no hay Opus"
    );

    client.send_control(&hello()).expect("hello");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo hello");
    control
        .poll(&mut server, &[session])
        .expect("control hello");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("respuesta hello");
    assert!(
        !control.opus_negotiated(&pin),
        "el hello del servidor no negocia Opus"
    );

    client
        .send_control(&capabilities(announced))
        .expect("capabilities del cliente");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo caps");
    control.poll(&mut server, &[session]).expect("control caps");
    assert_eq!(
        control.opus_negotiated(&pin),
        announced,
        "la capacidad sigue exactamente lo anunciado"
    );

    client
        .send_audio_payload(&opus_payload())
        .expect("frame opus al daemon");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 4).expect("vuelo audio");
    let received = server
        .recv_audio_payload(session)
        .expect("el transporte entrega el frame opus");
    assert_eq!(received.frame_type(), 2, "el frame viaja como type=2");

    let mut audio = SessionAudio::new(control.opus_negotiated(&pin));
    let decoded = audio.handle(received, 0).expect("decode");
    (
        audio,
        decoded.iter().map(|frame| frame.samples.len() as u64).sum(),
    )
}

/// Con `capabilities` que anuncian Opus, el frame se decodifica a muestras.
#[test]
fn client_capabilities_enable_opus_and_frames_decode() {
    let (audio, samples) = run(true);
    let stats = audio.stats();
    assert_eq!(stats.opus_frames, 1, "un frame opus decodificado");
    assert_eq!(stats.dropped_unnegotiated_opus, 0);
    assert!(samples > 0, "el frame produjo muestras");
}

/// Sin el anuncio, el mismo frame se descarta y se cuenta.
#[test]
fn missing_capabilities_drops_opus() {
    let (audio, samples) = run(false);
    let stats = audio.stats();
    assert_eq!(
        stats.dropped_unnegotiated_opus, 1,
        "descartado por no negociar"
    );
    assert_eq!(stats.opus_frames, 0, "no se decodifica");
    assert_eq!(samples, 0, "no produce muestras");
}
