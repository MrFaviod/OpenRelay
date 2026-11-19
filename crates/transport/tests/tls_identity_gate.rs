//! Gate de identidad TLS de sesión: mTLS real, pin SPKI, ACL default deny,
//! ALPN y DATAGRAM/stream de extremo a extremo sobre paquetes QUIC reales.

mod common;

use std::time::Instant;

use openrelay_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};
use openrelay_proto::wire::ControlMessage;
use openrelay_transport::quinn::{
    Acl, Capabilities, RejectReason, ServerEvent, SessionClient, SessionLimits, SessionServer,
    TransportError,
};

use common::{exchange, roots, session_setup, TestCa, CLIENT_ADDR, SERVER_ADDR};

fn authorized_session(server: &mut SessionServer) -> Option<openrelay_transport::quinn::SessionId> {
    while let Some(event) = server.next_event() {
        if let ServerEvent::Authorized { session, .. } = event {
            return Some(session);
        }
    }
    None
}

fn server_was_authorized(server: &mut SessionServer) -> bool {
    authorized_session(server).is_some()
}

#[test]
fn authorized_session_delivers_real_audio_and_control() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon", true, false);
    let client_leaf = ca.leaf("openrelay-android", false, false);
    let (mut client, mut server) =
        session_setup(&client_leaf, &server_leaf, &ca, Some(&client_leaf))
            .expect("configuración de sesión");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake QUIC");
    let session = authorized_session(&mut server).expect("sesión autorizada en el servidor");
    assert!(client.is_authorized(), "el cliente validó cadena y pin");
    assert_eq!(client.server_pin(), Some(server_leaf.pin));
    assert!(!client.has_0rtt(), "0-RTT nunca se ofrece");
    assert!(!client.accepted_0rtt(), "0-RTT nunca se acepta");
    assert!(
        client_leaf.signer.signatures() > 0,
        "el signer externo del cliente firmó el CertificateVerify"
    );
    assert!(
        server_leaf.signer.signatures() > 0,
        "el signer del servidor firmó el CertificateVerify"
    );
    assert_eq!(server.identity(session), Some(client_leaf.pin));
    assert_eq!(
        server.capabilities(session),
        Some(
            Capabilities::none()
                .with_mic()
                .with_control_read()
                .with_control_write()
        )
    );

    let mut samples = [0i16; SAMPLE_COUNT];
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample = (index as i16) - 120;
    }
    let frame = AudioFrame::new(1, 7, Flags::NONE, samples).expect("frame v1");
    client.send_audio(&frame).expect("DATAGRAM de audio");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("audio en vuelo");

    let received = server
        .recv_audio(session)
        .expect("audio entregado tras el gate");
    assert_eq!(received, frame);
    let encoded = received.encode();
    assert_eq!(
        encoded,
        frame.encode(),
        "roundtrip de encode, no sólo decode"
    );
    assert_eq!(
        AudioFrame::decode(&encoded).expect("decode del encode"),
        frame
    );

    let ping = ControlMessage::Ping {
        request_id: "t10-ping-1".to_string(),
    };
    client
        .send_control(&ping)
        .expect("control cliente→servidor");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("control en vuelo");
    assert_eq!(server.recv_control(session), Some(ping.clone()));

    let event = ControlMessage::Event(openrelay_proto::wire::EventMsg {
        revision: 1,
        event: openrelay_proto::wire::EventBody::SessionState {
            state: openrelay_proto::control::SessionState::Active,
            detail: None,
        },
    });
    server
        .send_control(session, &event)
        .expect("control servidor→cliente");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("evento en vuelo");
    assert_eq!(client.recv_control(), Some(event));
}

#[test]
fn session_without_client_certificate_is_rejected() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon", true, false);
    let mut acl = Acl::new();
    acl.allow(
        ca.leaf("openrelay-android", false, false).spki_sha256(),
        Capabilities::none().with_mic(),
    );
    let mut server = SessionServer::new(
        server_leaf.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión");
    let mut client = SessionClient::connect_without_client_identity(
        roots(&ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente sin identidad");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(
        !server_was_authorized(&mut server),
        "sin cert no hay sesión"
    );
    assert!(!client.is_authorized());
}

#[test]
fn valid_certificate_without_acl_is_denied() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon", true, false);
    let client_leaf = ca.leaf("openrelay-android", false, false);
    let (mut client, mut server) =
        session_setup(&client_leaf, &server_leaf, &ca, None).expect("configuración de sesión");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    let mut rejected = None;
    while let Some(event) = server.next_event() {
        if let ServerEvent::Rejected { reason } = event {
            rejected = Some(reason);
        }
    }
    assert_eq!(
        rejected,
        Some(RejectReason::NoAclEntry),
        "sin entrada ACL se deniega antes de datos"
    );
    assert!(!client.is_authorized());
    let frame = AudioFrame::new(1, 0, Flags::NONE, [0i16; SAMPLE_COUNT]).expect("frame v1");
    assert!(matches!(
        client.send_audio(&frame),
        Err(TransportError::Unauthorized)
    ));
}

#[test]
fn wrong_server_pin_fails_before_authorization() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-daemon", true, false);
    let impostor = ca.leaf("openrelay-impostor", true, false);
    let client_leaf = ca.leaf("openrelay-android", false, false);
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, Capabilities::none().with_mic());
    let mut server = SessionServer::new(
        server_leaf.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");
    let mut client = SessionClient::connect(
        client_leaf.certified(&ca),
        roots(&ca),
        impostor.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente con pin equivocado");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(
        !server_was_authorized(&mut server),
        "pin erróneo no autoriza"
    );
    assert!(
        !client.is_authorized(),
        "el cliente aborta en verify_server_cert"
    );
}

#[test]
fn expired_server_certificate_is_rejected() {
    let ca = TestCa::new();
    let expired = ca.leaf("openrelay-daemon-expired", true, true);
    let client_leaf = ca.leaf("openrelay-android", false, false);
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, Capabilities::none().with_mic());
    let mut server = SessionServer::new(
        expired.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor con cert caducado");
    let mut client = SessionClient::connect(
        client_leaf.certified(&ca),
        roots(&ca),
        expired.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(!server_was_authorized(&mut server));
    assert!(!client.is_authorized(), "webpki rechaza la validez");
}

#[test]
fn enrollment_anchor_does_not_authorize_session() {
    let session_ca = TestCa::new();
    let enroll_ca = TestCa::new();
    let server_leaf = session_ca.leaf("openrelay-daemon", true, false);
    let client_leaf = enroll_ca.leaf("openrelay-android", false, false);
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, Capabilities::none().with_mic());
    let mut server = SessionServer::new(
        server_leaf.certified(&session_ca),
        roots(&session_ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");

    // El cliente confía en el ancla de enrolamiento, no en la CA de sesión.
    let mut client = SessionClient::connect(
        client_leaf.certified(&enroll_ca),
        roots(&enroll_ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente con ancla de enroll");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(!server_was_authorized(&mut server));
    assert!(
        !client.is_authorized(),
        "el servidor no está bajo el ancla enroll"
    );
}

#[test]
fn client_certificate_from_foreign_ca_is_rejected() {
    let session_ca = TestCa::new();
    let foreign_ca = TestCa::new();
    let server_leaf = session_ca.leaf("openrelay-daemon", true, false);
    let client_leaf = foreign_ca.leaf("openrelay-android", false, false);
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, Capabilities::none().with_mic());
    let mut server = SessionServer::new(
        server_leaf.certified(&session_ca),
        roots(&session_ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");
    let mut client = SessionClient::connect(
        client_leaf.certified(&foreign_ca),
        roots(&session_ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(
        !server_was_authorized(&mut server),
        "CA ajena no entra en sesión"
    );
    assert!(!client.is_authorized());
}
