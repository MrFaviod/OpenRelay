//! Regresiones de stream: bytes ya disponibles en el momento de aceptar.
//!
//! Quinn emite `StreamEvent::Opened` una sola vez por stream remoto y **no**
//! vuelve a emitir `Readable` por bytes que ya estaban en el buffer de recepción
//! cuando el stream se aceptó. Por eso la lectura no puede depender de un
//! `Readable` posterior: al aceptar hay que drenar lo que ya llegó.
//!
//! Estos casos entregan el frame en **un único** vuelo cliente→servidor, de modo
//! que el paquete que abre el stream trae también el payload. Si la
//! implementación espera un `Readable`, el mensaje nunca se entrega.

mod common;

use std::time::Instant;

use common::*;
use openrelay_proto::wire::ControlMessage;
use openrelay_transport::enrollment::EnrollmentEvent;

/// Control: el primer `send_control` tras autorizar abre el stream y manda el
/// frame en el mismo vuelo. Un solo `drain` basta para que el servidor tenga el
/// stream abierto **con bytes ya disponibles**.
#[test]
fn fresh_control_stream_with_buffered_bytes_is_delivered() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server", true, false);
    let client_leaf = ca.leaf("openrelay-client", false, false);
    let (mut client, mut server) =
        session_setup(&client_leaf, &server_leaf, &ca, Some(&client_leaf)).expect("sesión");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    let session = server
        .next_event()
        .and_then(|event| match event {
            openrelay_transport::quinn::ServerEvent::Authorized { session, .. } => Some(session),
            _ => None,
        })
        .expect("sesión autorizada");

    let ping = ControlMessage::Ping {
        request_id: "reg-fresh-1".to_string(),
    };
    client
        .send_control(&ping)
        .expect("control cliente→servidor");
    // Un único vuelo: abre el stream y entrega el frame juntos.
    drain(&mut client, CLIENT_ADDR, &mut server, Instant::now()).expect("vuelo único");

    assert_eq!(
        server.recv_control(session),
        Some(ping),
        "el frame que llegó junto al Opened debe entregarse sin esperar un Readable"
    );
}

/// Frontera de enrolamiento: el token llega en el mismo paquete que abre el
/// stream, sin `Readable` posterior.
#[test]
fn fresh_enrollment_stream_with_buffered_bytes_is_accepted() {
    let ca = TestCa::new();
    let window_token = token();
    let (mut client, mut server) =
        enrollment_setup(&ca, window_token.clone()).expect("configuración de enroll");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    assert!(client.is_ready(), "cliente listo tras validar pin y ALPN");
    assert!(server.is_ready(), "servidor listo sin cert de cliente");

    client
        .send_token(&window_token, &client_spki_der())
        .expect("token");
    drain(&mut client, CLIENT_ADDR, &mut server, Instant::now()).expect("vuelo único");

    let mut accepted = false;
    while let Some(event) = server.next_event() {
        if matches!(event, EnrollmentEvent::Accepted(_)) {
            accepted = true;
        }
    }
    assert!(
        accepted,
        "el token que llegó junto al Opened debe aceptarse sin esperar un Readable"
    );
    assert_eq!(server.attempts(), 1, "un único intento");
}
