//! Gate de capacidades parciales de la ACL de sesión: cada permiso
//! (`mic`, `control_read`, `control_write`, `return_audio`) se prueba por
//! separado sobre un handshake mTLS real, con controles positivos y negativos
//! en ambos sentidos.
//!
//! Contrato observado en `crates/transport/src/quinn.rs`:
//! - `SessionServer::recv_audio` exige `mic` (audio cliente→servidor).
//! - `SessionServer::send_audio` exige `return_audio` (audio servidor→cliente).
//! - `SessionServer::send_control` exige `control_read` (mensajes que el
//!   servidor **lee** del cliente según `docs/protocol.md`).
//! - `SessionServer::recv_control` exige `control_write`.
//!
//! El cliente no recibe la ACL remota (`ClientEvent::Authorized` sólo trae el
//! pin del servidor), así que una operación prohibida en sentido
//! cliente→servidor se afirma en el lado servidor (`recv_*` = `None`, sin
//! cierre ni estado residual) y en sentido servidor→cliente en el `Err`
//! `Capability(..)` que devuelve la propia API del servidor.

mod common;

use std::time::Instant;

use arl_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};
use arl_proto::wire::ControlMessage;
use arl_transport::quinn::{
    Acl, Capabilities, SessionClient, SessionId, SessionLimits, SessionServer, TransportError,
};

use common::{exchange, roots, TestCa, TestLeaf, CLIENT_ADDR, SERVER_ADDR};

/// Sesión autorizada del servidor, si el handshake la concedió.
fn authorized_session(server: &mut SessionServer) -> Option<SessionId> {
    while let Some(event) = server.next_event() {
        if let arl_transport::quinn::ServerEvent::Authorized { session, .. } = event {
            return Some(session);
        }
    }
    None
}

/// Servidor con una única entrada ACL que concede exactamente `caps`.
fn server_with_caps(
    server_leaf: &TestLeaf,
    client_leaf: &TestLeaf,
    ca: &TestCa,
    caps: Capabilities,
) -> SessionServer {
    let mut acl = Acl::new();
    acl.allow(client_leaf.spki_sha256(), caps);
    SessionServer::new(
        server_leaf.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión con ACL parcial")
}

fn client_for(client_leaf: &TestLeaf, server_leaf: &TestLeaf, ca: &TestCa) -> SessionClient {
    SessionClient::connect(
        client_leaf.certified(ca),
        roots(ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión")
}

fn frame(seq: u32) -> AudioFrame {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample = (index as i16) - 120 + seq as i16;
    }
    AudioFrame::new(1, seq, Flags::NONE, samples).expect("frame v1")
}

fn ping(request_id: &str) -> ControlMessage {
    ControlMessage::Ping {
        request_id: request_id.to_string(),
    }
}

/// `mic` concedido: el audio del cliente se entrega, y toda operación del
/// servidor que exige otra capacidad falla sin tocar el estado de la sesión.
#[test]
fn mic_only_delivers_client_audio_and_denies_the_rest() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-daemon-cap-mic", true, false);
    let client_leaf = ca.leaf("arl-client-cap-mic", false, false);
    let mut server = server_with_caps(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_mic(),
    );
    let mut client = client_for(&client_leaf, &server_leaf, &ca);

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada");
    assert_eq!(
        server.capabilities(session),
        Some(Capabilities::none().with_mic()),
        "el servidor expone exactamente la capacidad concedida"
    );

    let audio = frame(1);
    client.send_audio(&audio).expect("cliente envía audio");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("audio en vuelo");
    assert_eq!(
        server.recv_audio(session),
        Some(audio.clone()),
        "control positivo: `mic` entrega el audio del cliente"
    );

    assert!(
        matches!(
            server.send_audio(session, &frame(2)),
            Err(TransportError::Capability("return_audio"))
        ),
        "falta de capacidad denegada"
    );
    assert!(
        matches!(
            server.send_control(session, &ping("cap-mic-server")),
            Err(TransportError::Capability("control.read"))
        ),
        "falta de capacidad denegada"
    );

    client
        .send_control(&ping("cap-mic-client"))
        .expect("el cliente no conoce la ACL remota y envía igual");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("control en vuelo");
    assert_eq!(
        server.recv_control(session),
        None,
        "sin `control_write` el mensaje del cliente no se entrega"
    );

    assert_eq!(
        server.identity(session),
        Some(client_leaf.spki_sha256()),
        "los rechazos por capacidad no revocan la sesión"
    );
    assert!(
        client.is_authorized(),
        "el pin del servidor sigue verificado tras los rechazos"
    );
}

/// `return_audio` concedido: el audio del servidor llega al cliente; el audio
/// del cliente, sin `mic`, no se entrega aunque su DATAGRAM se reciba.
#[test]
fn return_audio_only_delivers_server_audio_and_drops_client_audio() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-daemon-cap-ret", true, false);
    let client_leaf = ca.leaf("arl-client-cap-ret", false, false);
    let mut server = server_with_caps(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_return_audio(),
    );
    let mut client = client_for(&client_leaf, &server_leaf, &ca);

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada");

    let audio = frame(3);
    server
        .send_audio(session, &audio)
        .expect("servidor emite audio");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("audio en vuelo");
    assert_eq!(
        client.recv_audio(),
        Some(audio),
        "control positivo: `return_audio` entrega el audio del servidor"
    );

    client
        .send_audio(&frame(4))
        .expect("el cliente no conoce la ACL remota y envía igual");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("audio en vuelo");
    assert_eq!(
        server.recv_audio(session),
        None,
        "sin `mic` el audio del cliente no se entrega"
    );
    assert!(
        matches!(
            server.send_control(session, &ping("cap-ret-server")),
            Err(TransportError::Capability("control.read"))
        ),
        "falta de capacidad denegada"
    );
    assert_eq!(server.identity(session), Some(client_leaf.spki_sha256()));
}

/// `control_write` concedido: el servidor recibe el control del cliente; no
/// puede enviar el suyo ni entregar audio.
#[test]
fn control_write_only_delivers_client_control() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-daemon-cap-cw", true, false);
    let client_leaf = ca.leaf("arl-client-cap-cw", false, false);
    let mut server = server_with_caps(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_control_write(),
    );
    let mut client = client_for(&client_leaf, &server_leaf, &ca);

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada");

    let message = ping("cap-cw-client");
    client
        .send_control(&message)
        .expect("cliente envía control");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 16).expect("control en vuelo");
    assert_eq!(
        server.recv_control(session),
        Some(message),
        "control positivo: `control_write` entrega el control del cliente"
    );

    assert!(
        matches!(
            server.send_control(session, &ping("cap-cw-server")),
            Err(TransportError::Capability("control.read"))
        ),
        "falta de capacidad denegada"
    );
    client.send_audio(&frame(5)).expect("DATAGRAM enviado");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("audio en vuelo");
    assert_eq!(server.recv_audio(session), None, "sin `mic` no hay audio");
    assert!(
        matches!(
            server.send_audio(session, &frame(6)),
            Err(TransportError::Capability("return_audio"))
        ),
        "falta de capacidad denegada"
    );
}

/// `control_read` concedido: el servidor envía control; el del cliente no se
/// entrega y el envío de audio sigue denegado en ambos sentidos.
#[test]
fn control_read_only_delivers_server_control() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-daemon-cap-cr", true, false);
    let client_leaf = ca.leaf("arl-client-cap-cr", false, false);
    let mut server = server_with_caps(
        &server_leaf,
        &client_leaf,
        &ca,
        Capabilities::none().with_control_read(),
    );
    let mut client = client_for(&client_leaf, &server_leaf, &ca);

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada");

    let message = ping("cap-cr-server");
    server
        .send_control(session, &message)
        .expect("servidor envía control");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 16).expect("control en vuelo");
    assert_eq!(
        client.recv_control(),
        Some(message),
        "control positivo: `control_read` entrega el control del servidor"
    );

    client
        .send_control(&ping("cap-cr-client"))
        .expect("el cliente envía sin conocer la ACL");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 16).expect("control en vuelo");
    assert_eq!(
        server.recv_control(session),
        None,
        "sin `control_write` el control del cliente no se entrega"
    );
    assert!(
        matches!(
            server.send_audio(session, &frame(7)),
            Err(TransportError::Capability("return_audio"))
        ),
        "falta de capacidad denegada"
    );
    assert_eq!(server.identity(session), Some(client_leaf.spki_sha256()));
}

/// Entrada ACL explícita sin permisos: la sesión se autoriza (la entrada
/// existe) pero las cuatro operaciones quedan denegadas en ambos sentidos y
/// nada muta el estado.
#[test]
fn explicit_entry_without_capabilities_authorizes_and_denies_everything() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-daemon-cap-none", true, false);
    let client_leaf = ca.leaf("arl-client-cap-none", false, false);
    let mut server = server_with_caps(&server_leaf, &client_leaf, &ca, Capabilities::none());
    let mut client = client_for(&client_leaf, &server_leaf, &ca);

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("entrada sin permisos autoriza");
    assert_eq!(
        server.capabilities(session),
        Some(Capabilities::none()),
        "capacidades vacías, no ausencia de entrada ACL"
    );
    assert_eq!(server.identity(session), Some(client_leaf.spki_sha256()));

    client.send_audio(&frame(8)).expect("DATAGRAM enviado");
    client
        .send_control(&ping("cap-none-client"))
        .expect("control enviado");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 16).expect("datos en vuelo");
    assert_eq!(server.recv_audio(session), None, "sin `mic` no hay audio");
    assert_eq!(
        server.recv_control(session),
        None,
        "sin `control_write` no hay control entrante"
    );
    assert!(
        matches!(
            server.send_audio(session, &frame(9)),
            Err(TransportError::Capability("return_audio"))
        ),
        "falta de capacidad denegada"
    );
    assert!(
        matches!(
            server.send_control(session, &ping("cap-none-server")),
            Err(TransportError::Capability("control.read"))
        ),
        "falta de capacidad denegada"
    );

    assert_eq!(
        server.identity(session),
        Some(client_leaf.spki_sha256()),
        "cuatro denegaciones seguidas no revocan la sesión"
    );
    assert_eq!(
        server.capabilities(session),
        Some(Capabilities::none()),
        "las capacidades no mutan al ser denegadas"
    );
    assert!(client.is_authorized(), "el cliente sigue autorizado");
}
