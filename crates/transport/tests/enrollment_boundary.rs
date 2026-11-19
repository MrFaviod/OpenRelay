//! Frontera de enrolamiento: ALPN distinto, sin certificado de cliente, sin
//! DATAGRAM/tickets/0-RTT, token de un solo uso y rechazo explícito de audio,
//! control y tickets.

mod common;

use std::time::{Duration, Instant};

use openrelay_transport::enrollment::{
    EnrollClient, EnrollReject, EnrollServer, EnrollmentEvent, EnrollmentToken,
};
use openrelay_transport::quinn::{Acl, Capabilities, SessionClient, SessionLimits, SessionServer};
use serde_json::json;

use common::{
    client_spki_der, enrollment_setup, exchange, roots, token, TestCa, CLIENT_ADDR, SERVER_ADDR,
};

fn collect_server_events(server: &mut EnrollServer) -> Vec<EnrollmentEvent> {
    let mut events = Vec::new();
    while let Some(event) = server.next_event() {
        events.push(event);
    }
    events
}

fn collect_client_events(client: &mut EnrollClient) -> Vec<EnrollmentEvent> {
    let mut events = Vec::new();
    while let Some(event) = client.next_event() {
        events.push(event);
    }
    events
}

fn has_forbidden(events: &[EnrollmentEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::ForbiddenSurface(_)))
}

#[test]
fn enrollment_accepts_valid_token_without_client_certificate() {
    let ca = TestCa::new();
    let window_token = token();
    let (mut client, mut server) =
        enrollment_setup(&ca, window_token.clone()).expect("configuración de enroll");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake enroll");
    assert!(client.is_ready(), "cliente listo tras validar pin y ALPN");
    assert!(server.is_ready(), "servidor listo sin cert de cliente");

    client
        .send_token_async(&window_token, &client_spki_der())
        .expect("token tras validar pin");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("token en vuelo");

    let server_events = collect_server_events(&mut server);
    assert!(server_events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Accepted(_))));

    // El resultado sólo se escribe después de la aprobación local.
    server
        .approve(
            Instant::now(),
            &openrelay_transport::enrollment::EnrollOutcome {
                client_leaf_der: vec![0x30, 0x01],
                session_ca_der: vec![0x30, 0x02],
                session_port: 47000,
            },
        )
        .expect("aprobación");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("resultado en vuelo");

    let client_events = collect_client_events(&mut client);
    assert!(client_events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Accepted(_))));
    assert_eq!(server.attempts(), 1, "un único intento de token");
}

#[test]
fn wrong_pin_aborts_before_sending_token() {
    let ca = TestCa::new();
    let leaf = ca.leaf("openrelay-enroll", true, false);
    let impostor = ca.leaf("openrelay-impostor", true, false);
    let mut server = EnrollServer::new(leaf.certified(&ca)).expect("servidor enroll");
    server
        .open_window(token(), Duration::from_secs(120), Instant::now())
        .expect("ventana");
    let mut client = EnrollClient::connect(
        roots(&ca),
        impostor.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente con pin equivocado");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    let events = collect_server_events(&mut server);
    assert!(!client.is_ready(), "el pin erróneo aborta el handshake");
    assert_eq!(
        server.attempts(),
        0,
        "el cliente no envía el token si el pin no coincide"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EnrollmentEvent::Accepted(_))),
        "sin aceptación con pin equivocado"
    );
}

#[test]
fn expired_enrollment_certificate_is_rejected() {
    let ca = TestCa::new();
    let expired = ca.leaf("openrelay-enroll-expired", true, true);
    let mut server = EnrollServer::new(expired.certified(&ca)).expect("servidor enroll");
    server
        .open_window(token(), Duration::from_secs(120), Instant::now())
        .expect("ventana");
    let mut client = EnrollClient::connect(
        roots(&ca),
        expired.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(!client.is_ready());
    assert_eq!(server.attempts(), 0);
}

#[test]
fn expired_window_is_rejected() {
    let ca = TestCa::new();
    let leaf = ca.leaf("openrelay-enroll", true, false);
    let window_token = token();
    let mut server = EnrollServer::new(leaf.certified(&ca)).expect("servidor enroll");
    // TTL mínimo no nulo, ya vencido al llegar el mensaje.
    server
        .open_window(
            window_token.clone(),
            Duration::from_nanos(1),
            Instant::now(),
        )
        .expect("ventana");
    let mut client = EnrollClient::connect(
        roots(&ca),
        leaf.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    client
        .send_token_async(&window_token, &client_spki_der())
        .expect("token");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("token en vuelo");

    let events = collect_server_events(&mut server);
    assert!(events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Rejected(EnrollReject::Expired))));
    assert!(!events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Accepted(_))));
}

#[test]
fn wrong_token_is_rejected() {
    let ca = TestCa::new();
    let window_token = token();
    let (mut client, mut server) =
        enrollment_setup(&ca, window_token.clone()).expect("configuración de enroll");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");

    let wrong = EnrollmentToken::generate().expect("token distinto");
    client
        .send_token_async(&wrong, &client_spki_der())
        .expect("token");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("token en vuelo");

    let events = collect_server_events(&mut server);
    assert!(events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Rejected(EnrollReject::Mismatch))));
    assert!(!events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Accepted(_))));
}

#[test]
fn audio_control_and_ticket_are_rejected_on_enroll() {
    for surface in ["audio", "control", "ticket"] {
        let ca = TestCa::new();
        let (mut client, mut server) =
            enrollment_setup(&ca, token()).expect("configuración de enroll");
        exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
        assert!(client.is_ready());

        client
            .send_unchecked_frame(&json!({"type": surface, "version": 1}))
            .expect("frame prohibido");
        exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("frame en vuelo");

        let events = collect_server_events(&mut server);
        assert!(
            has_forbidden(&events),
            "el servidor debe registrar la superficie {surface} como prohibida"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EnrollmentEvent::Accepted(_))),
            "{surface} no puede aceptar enrolamiento"
        );
    }
}

#[test]
fn session_client_cannot_use_enrollment_endpoint() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-enroll", true, false);
    let client_leaf = ca.leaf("openrelay-android", false, false);
    let mut server = EnrollServer::new(server_leaf.certified(&ca)).expect("servidor enroll");
    server
        .open_window(token(), Duration::from_secs(120), Instant::now())
        .expect("ventana");
    let mut client = SessionClient::connect(
        client_leaf.certified(&ca),
        roots(&ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(!client.is_authorized());
    assert!(!server.is_ready(), "ALPN de sesión no entra en enroll");
    assert_eq!(server.attempts(), 0);
}

#[test]
fn enrollment_client_cannot_use_session_endpoint() {
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
    let mut client = EnrollClient::connect(
        roots(&ca),
        server_leaf.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente enroll");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake intentado");
    assert!(
        !client.is_ready(),
        "ALPN de enrolamiento no entra en sesión"
    );
    let mut authorized = false;
    while let Some(event) = server.next_event() {
        if matches!(
            event,
            openrelay_transport::quinn::ServerEvent::Authorized { .. }
        ) {
            authorized = true;
        }
    }
    assert!(!authorized);
}
