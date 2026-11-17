//! Regresiones B sobre `Connection` QUIC real: correlación del `error` de
//! control y contrapresión de ventana.
//!
//! Módulo hijo de `quinn_regressions`: reutiliza sus fixtures test-only (CA,
//! hojas ECDSA P-256, ACL, enlace en memoria con datagramas QUIC reales) y
//! añade un peer crudo con `TransportConfig` propio, para poder anunciar una
//! ventana de recepción pequeña de verdad. No hay mocks: el peer hace el
//! handshake mTLS con ALPN `arl-session/1` y pin SPKI, igual que el cliente
//! real.
//!
//! Los casos describen el contrato de `docs/protocol.md` §3 y de
//! `arl_proto::wire`: un frame con framing íntegro y `request_id` recuperable
//! se responde con `error` en el mismo stream y la sesión sigue viva; un frame
//! con framing corrupto o `request_id` irrecuperable cierra la conexión; un
//! `error` inválido se descarta sin responder con otro `error`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use arl_proto::control::{ErrorCode, ErrorMsg};
use arl_proto::wire::{self, ControlMessage};
use quinn_proto::crypto::rustls::QuicClientConfig;
use quinn_proto::{
    ClientConfig as QuinnClientConfig, Connection, Dir, Endpoint, EndpointConfig, ReadError,
    StreamId, TransportConfig, VarInt,
};
use rustls::ClientConfig as TlsClientConfig;

use super::{
    authorized_session, exchange, roots, server_for, Capabilities, RawPeer, TestCa, TestLeaf,
    CLOSE_CONTROL_FRAMING, SERVER_ADDR,
};
use crate::identity::{PinnedServerVerifier, StaticClientCertResolver};
use crate::ALPN_SESSION;

/// Ventana de recepción anunciada por el peer en el caso B3, en bytes.
///
/// Es un valor real de `TransportConfig::stream_receive_window`: viaja en los
/// transport parameters del handshake y es el techo que QUIC impone a lo que el
/// receptor puede escribir de vuelta en el stream del peer.
const SMALL_STREAM_WINDOW: u32 = 64;

/// Margen de rondas para que el peer libere crédito y el receptor reanude.
const RESUME_ROUNDS: usize = 512;

// ---------------------------------------------------------------------------
// Peer crudo con ventana propia.
// ---------------------------------------------------------------------------

/// Igual que [`RawPeer::connect`], pero con `stream_receive_window` explícita.
///
/// `RawPeer::connect` usa `TransportConfig::default()` (ventana de 1,25 MB), que
/// nunca se agota con un frame de error. Aquí el peer anuncia una ventana
/// pequeña real para que la escritura de la respuesta se trunque por control de
/// flujo del propio protocolo, no por una simulación.
fn raw_peer_with_stream_window(
    client_leaf: &TestLeaf,
    server_leaf: &TestLeaf,
    ca: &TestCa,
    now: Instant,
    stream_window: u32,
) -> RawPeer {
    let verifier =
        PinnedServerVerifier::new(roots(ca), server_leaf.pin).expect("verificador pinneado");
    let mut tls = TlsClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_cert_resolver(Arc::new(StaticClientCertResolver::new(
            client_leaf.certified(ca),
        )));
    tls.alpn_protocols = vec![ALPN_SESSION.to_vec()];
    tls.enable_early_data = false;
    let crypto = QuicClientConfig::try_from(tls).expect("crypto de cliente");
    let mut quinn = QuinnClientConfig::new(Arc::new(crypto));
    let mut transport = TransportConfig::default();
    transport.datagram_receive_buffer_size(Some(4_096));
    transport.datagram_send_buffer_size(4_096);
    // Ventana pequeña real: el receptor sólo podrá escribir esto en el stream
    // que el peer abra antes de quedarse sin crédito.
    transport.stream_receive_window(VarInt::from_u32(stream_window));
    transport.receive_window(VarInt::from_u32(stream_window * 64));
    quinn.transport_config(Arc::new(transport));
    let mut endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None);
    let (handle, conn) = endpoint
        .connect(now, quinn, SERVER_ADDR, "localhost")
        .expect("conexión cruda");
    RawPeer {
        endpoint,
        conn,
        handle,
        outgoing: VecDeque::new(),
        scratch: Vec::new(),
        application_close: None,
    }
}

// ---------------------------------------------------------------------------
// Utilidades de frame crudo.
// ---------------------------------------------------------------------------

/// Antepone el prefijo `u32` BE correcto al payload dado.
fn raw_frame(payload: &str) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(payload.as_bytes());
    frame
}

/// Escribe el frame completo en `stream`, sin admitir bloqueo silencioso.
fn write_frame(conn: &mut Connection, stream: StreamId, frame: &[u8]) {
    let mut written = 0usize;
    while written < frame.len() {
        match conn.send_stream(stream).write(&frame[written..]) {
            Ok(0) => panic!("escritura bloqueada sin progreso"),
            Ok(count) => written += count,
            Err(error) => panic!("escritura cruda rechazada: {error:?}"),
        }
    }
    assert_eq!(written, frame.len(), "frame crudo completo en el stream");
}

/// Drena lo que el peer tenga disponible en `stream` (sin bloquear).
fn read_available(conn: &mut Connection, stream: StreamId) -> Vec<u8> {
    let mut out = Vec::new();
    let mut recv = conn.recv_stream(stream);
    let Ok(mut chunks) = recv.read(true) else {
        return out;
    };
    loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => out.extend_from_slice(&chunk.bytes),
            Ok(None) => break,
            Err(ReadError::Blocked) => break,
            Err(ReadError::Reset(_)) => break,
        }
    }
    let _ = chunks.finalize();
    out
}

/// `error` que el contrato exige para un frame de forma inválida con id válido.
fn expected_invalid_error(request_id: &str) -> Vec<u8> {
    let message = ErrorMsg::new(request_id, ErrorCode::Invalid, "invalid control message")
        .expect("error de contrato construible");
    wire::encode_message(&ControlMessage::Error(message)).expect("error codificable")
}

/// Comprueba que la respuesta es exactamente un `error invalid` con `request_id`.
fn assert_single_invalid_error(bytes: &[u8], request_id: &str) {
    let decoded =
        wire::decode_message(bytes).expect("la respuesta debe ser un frame de control válido");
    match decoded {
        ControlMessage::Error(error) => {
            assert_eq!(
                error.request_id, request_id,
                "el error conserva el request_id"
            );
            assert_eq!(error.code, ErrorCode::Invalid, "el código es invalid");
        }
        other => panic!("la respuesta debe ser `error`, no {other:?}"),
    }
    assert_eq!(
        bytes,
        expected_invalid_error(request_id),
        "la respuesta es exactamente el error de contrato"
    );
}

// ---------------------------------------------------------------------------
// B1: error correlacionado, misma sesión, mismo stream.
// ---------------------------------------------------------------------------

/// B1a: `type` desconocido con `request_id` válido y prefijo correcto.
/// El receptor responde `error invalid` con el MISMO id en el MISMO stream, no
/// entrega el mensaje inválido, la sesión sigue viva y el siguiente `ping`
/// válido se entrega.
#[test]
fn b1_invalid_type_with_valid_id_gets_error_on_same_stream() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-1a", true);
    let client_leaf = ca.leaf("arl-client-t10b-1a", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(peer.is_authorized(), "peer con handshake completo");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    let frame = raw_frame(
        r#"{"schema_version":"arl.control/v1","type":"config.nope","request_id":"b1a-tipo"}"#,
    );
    write_frame(&mut peer.conn, stream, &frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del frame inválido");

    assert_eq!(
        server.recv_control(session),
        None,
        "el mensaje inválido no se entrega"
    );
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "la sesión sigue autorizada tras un frame recuperable"
    );
    assert!(
        peer.application_close.is_none(),
        "no hay cierre por un frame de forma inválida con id recuperable"
    );

    let reply = read_available(&mut peer.conn, stream);
    assert!(!reply.is_empty(), "el receptor responde en el mismo stream");
    assert_single_invalid_error(&reply, "b1a-tipo");

    let ping = ControlMessage::Ping {
        request_id: "b1a-ping".to_string(),
    };
    let ping_frame = wire::encode_message(&ping).expect("ping codificable");
    write_frame(&mut peer.conn, stream, &ping_frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del ping válido");

    assert_eq!(
        server.recv_control(session),
        Some(ping),
        "el ping posterior se entrega en la misma sesión"
    );
}

/// B1b: `type` válido con campo obligatorio ausente (`config.get` sin
/// `payload`), mismo `request_id` recuperable. Misma respuesta correlacionada.
#[test]
fn b1_missing_required_field_gets_error_on_same_stream() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-1b", true);
    let client_leaf = ca.leaf("arl-client-t10b-1b", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    let frame = raw_frame(
        r#"{"schema_version":"arl.control/v1","type":"config.get","request_id":"b1b-campo"}"#,
    );
    write_frame(&mut peer.conn, stream, &frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del frame incompleto");

    assert_eq!(
        server.recv_control(session),
        None,
        "el mensaje sin campo obligatorio no se entrega"
    );
    assert_eq!(
        server.capabilities(session),
        Some(
            Capabilities::none()
                .with_mic()
                .with_control_read()
                .with_control_write()
        ),
        "ni muta capacidades"
    );
    assert!(
        peer.application_close.is_none(),
        "no hay cierre por un campo obligatorio ausente con id recuperable"
    );

    let reply = read_available(&mut peer.conn, stream);
    assert!(!reply.is_empty(), "el receptor responde en el mismo stream");
    assert_single_invalid_error(&reply, "b1b-campo");

    let ping = ControlMessage::Ping {
        request_id: "b1b-ping".to_string(),
    };
    let ping_frame = wire::encode_message(&ping).expect("ping codificable");
    write_frame(&mut peer.conn, stream, &ping_frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del ping válido");

    assert_eq!(
        server.recv_control(session),
        Some(ping),
        "el ping posterior se entrega en la misma sesión"
    );
}

// ---------------------------------------------------------------------------
// B2: cierre conforme a `WireError::closes_connection`, sin id inventado.
// ---------------------------------------------------------------------------

/// B2a: JSON corrupto con prefijo íntegro. `parse_json` falla con
/// `MalformedJson(None)` → `closes_connection()` es `true`: se cierra `0x104` y
/// no se responde con ningún `request_id` inventado.
#[test]
fn b2_corrupt_json_closes_without_inventing_request_id() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-2a", true);
    let client_leaf = ca.leaf("arl-client-t10b-2a", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    write_frame(&mut peer.conn, stream, &raw_frame("{not-json"));
    exchange(&mut peer, &mut server, 32).expect("bombeo del JSON corrupto");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "JSON corrupto cierra 0x104 (closes_connection == true)"
    );
    assert_eq!(
        server.identity(session),
        None,
        "sin identidad residual tras el cierre"
    );
    assert!(
        read_available(&mut peer.conn, stream).is_empty(),
        "no se inventa un request_id: no hay respuesta de error"
    );
}

/// B2b: `request_id` presente pero irrecuperable (formato inválido) y `type`
/// desconocido. `peek_request_id` devuelve `None` → `InvalidShape(None)` →
/// `closes_connection()` es `true`: cierra `0x104` sin inventar id.
#[test]
fn b2_unrecoverable_request_id_closes_without_inventing_request_id() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-2b", true);
    let client_leaf = ca.leaf("arl-client-t10b-2b", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    let frame = raw_frame(
        r#"{"schema_version":"arl.control/v1","type":"config.nope","request_id":"id con espacios"}"#,
    );
    write_frame(&mut peer.conn, stream, &frame);
    exchange(&mut peer, &mut server, 32).expect("bombeo del id irrecuperable");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "un id irrecuperable cierra 0x104 (InvalidShape sin request_id)"
    );
    assert_eq!(
        server.identity(session),
        None,
        "sin identidad residual tras el cierre"
    );
    assert!(
        read_available(&mut peer.conn, stream).is_empty(),
        "no se inventa un request_id a partir de un valor inválido"
    );
}

// ---------------------------------------------------------------------------
// B3: contrapresión real de ventana sobre la respuesta de error.
// ---------------------------------------------------------------------------

/// B3 precondición: el fixture bloquea de verdad.
///
/// Con el peer escribiendo 40 000 bytes en un stream que el servidor nunca lee,
/// la ventana que el servidor anuncia (`stream_receive_window` = 32 768, fijada
/// por `transport_config` en `quinn.rs`) acepta sólo los primeros 32 768 y
/// después devuelve `Err(WriteError::Blocked)` real de QUIC. No hay mock ni
/// `Ok(0)` simulado: es la misma contrapresión que sufre la respuesta de error.
#[test]
fn b3_real_flow_control_accepts_partial_write_then_blocks() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-3p", true);
    let client_leaf = ca.leaf("arl-client-t10b-3p", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(peer.is_authorized(), "peer con handshake completo");
    let _session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    let payload = vec![0xA5u8; 40_000];
    let mut written = 0usize;
    let blocked = loop {
        match peer.conn.send_stream(stream).write(&payload[written..]) {
            Ok(0) => panic!("Ok(0) no es una precondición válida"),
            Ok(count) => written += count,
            Err(quinn_proto::WriteError::Blocked) => break true,
            Err(error) => panic!("error inesperado: {error:?}"),
        }
    };

    assert!(
        blocked,
        "el fixture debe producir Err(WriteError::Blocked) real, no un bloqueo simulado"
    );
    assert_eq!(
        written, 32_768,
        "la ventana anunciada por el servidor acepta exactamente 32 768 bytes antes de bloquear"
    );
    assert!(
        written < payload.len(),
        "escritura parcial real: {} de {} bytes",
        written,
        payload.len()
    );
}

/// B3 control: con ventana amplia el error llega íntegro y la sesión sigue viva.
///
/// Es el mismo caso que B3 pero con `stream_receive_window` de 64 KB. Si este
/// control se entrega y el caso pequeño no, la diferencia es atribuible a la
/// ventana anunciada y no a otra causa del fixture.
#[test]
fn b3_control_wide_window_delivers_error_reply_whole() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-3c", true);
    let client_leaf = ca.leaf("arl-client-t10b-3c", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer =
        raw_peer_with_stream_window(&client_leaf, &server_leaf, &ca, Instant::now(), 65_536);

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let expected = expected_invalid_error("b3-control");
    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    write_frame(
        &mut peer.conn,
        stream,
        &raw_frame(
            r#"{"schema_version":"arl.control/v1","type":"config.nope","request_id":"b3-control"}"#,
        ),
    );
    exchange(&mut peer, &mut server, 8).expect("bombeo del frame inválido");

    let received = read_available(&mut peer.conn, stream);
    assert_eq!(
        received, expected,
        "con ventana amplia la respuesta de error llega íntegra"
    );
    assert!(
        peer.application_close.is_none(),
        "con ventana amplia no hay cierre 0x104"
    );
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "la sesión sigue autorizada"
    );
}

/// **B3 (RED)**: el peer anuncia `stream_receive_window = 64` en su
/// `TransportConfig` real y envía un frame inválido recuperable. La respuesta
/// `error` de 163 bytes no cabe en esa ventana.
///
/// El contrato exige que la respuesta **espere** crédito y se reintente al
/// recibir `Writable`, sin cerrar `0x104`, y que al liberar el crédito llegue
/// íntegra y una sola vez. La precondición de bloqueo está probada aparte por
/// [`b3_real_flow_control_accepts_partial_write_then_blocks`] y la causalidad
/// por [`b3_control_wide_window_delivers_error_reply_whole`]; aquí se mide el
/// desenlace observable.
#[test]
fn b3_small_window_reply_waits_for_credit_instead_of_closing_0x104() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-3", true);
    let client_leaf = ca.leaf("arl-client-t10b-3", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = raw_peer_with_stream_window(
        &client_leaf,
        &server_leaf,
        &ca,
        Instant::now(),
        SMALL_STREAM_WINDOW,
    );

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(peer.is_authorized(), "peer con handshake completo");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let expected = expected_invalid_error("b3-ventana");
    assert!(
        expected.len() as u32 > SMALL_STREAM_WINDOW,
        "el frame de error debe exceder la ventana anunciada ({} > {})",
        expected.len(),
        SMALL_STREAM_WINDOW
    );

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    write_frame(
        &mut peer.conn,
        stream,
        &raw_frame(
            r#"{"schema_version":"arl.control/v1","type":"config.nope","request_id":"b3-ventana"}"#,
        ),
    );
    exchange(&mut peer, &mut server, 8).expect("bombeo del frame inválido");

    // El receptor no puede completar la respuesta de una vez: debe esperar.
    assert!(
        peer.application_close.is_none(),
        "la respuesta bloqueada por ventana no puede cerrar 0x104"
    );
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "la sesión sigue viva mientras la respuesta espera crédito"
    );

    // Liberar crédito leyendo y dejar que QUIC emita MAX_STREAM_DATA.
    let mut received = read_available(&mut peer.conn, stream);
    for _ in 0..RESUME_ROUNDS {
        if received.len() == expected.len() {
            break;
        }
        exchange(&mut peer, &mut server, 1).expect("bombeo tras liberar crédito");
        received.extend_from_slice(&read_available(&mut peer.conn, stream));
        assert!(
            peer.application_close.is_none(),
            "la sesión no puede cerrarse mientras libera crédito"
        );
    }

    assert_eq!(
        received, expected,
        "la respuesta llega íntegra, una sola vez, tras liberar el crédito"
    );

    // Sin duplicados: nuevas rondas no añaden bytes.
    exchange(&mut peer, &mut server, 4).expect("bombeo posterior");
    assert!(
        read_available(&mut peer.conn, stream).is_empty(),
        "la respuesta no se duplica tras reanudar"
    );
}

// ---------------------------------------------------------------------------
// B4: un `error` inválido no dispara error→error.
// ---------------------------------------------------------------------------

/// B4: frame con `type: "error"` cuyo `payload.retryable` contradice al código.
/// `decode_message` falla con `InvalidShape` y `request_id` recuperable;
/// `is_error_frame` es cierto, así que se descarta en silencio: ni respuesta
/// `error`, ni cierre, ni bucle. El stream sigue utilizable.
#[test]
fn b4_invalid_error_frame_is_discarded_without_error_loop() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-4", true);
    let client_leaf = ca.leaf("arl-client-t10b-4", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    // `invalid` no es retryable: `retryable: true` es una forma inválida.
    let frame = raw_frame(
        r#"{"schema_version":"arl.control/v1","type":"error","request_id":"b4-bucle","payload":{"code":"invalid","message":"x","retryable":true}}"#,
    );
    write_frame(&mut peer.conn, stream, &frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del error inválido");

    assert!(
        peer.application_close.is_none(),
        "un error inválido con id recuperable no cierra la conexión"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "el error inválido no se entrega como mensaje"
    );
    assert!(
        read_available(&mut peer.conn, stream).is_empty(),
        "no se responde error a un error: sin bucle error→error"
    );

    let ping = ControlMessage::Ping {
        request_id: "b4-ping".to_string(),
    };
    let ping_frame = wire::encode_message(&ping).expect("ping codificable");
    write_frame(&mut peer.conn, stream, &ping_frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del ping válido");

    assert_eq!(
        server.recv_control(session),
        Some(ping),
        "el framing sigue íntegro: el siguiente ping se entrega"
    );
}

#[path = "quinn_regressions_b_lifecycle.rs"]
mod lifecycle;

/// B4: contrapresión real. El peer anuncia una ventana de 64 bytes y no lee
/// nunca: QUIC bloquea, la cola de respuestas se llena y `send_control`
/// devuelve `QueueFull` decidido antes de escribir el frame que lo provoca.
#[test]
fn b4_queue_full_is_reached_under_real_backpressure() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-server-t10b-4q", true);
    let client_leaf = ca.leaf("arl-client-t10b-4q", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = raw_peer_with_stream_window(
        &client_leaf,
        &server_leaf,
        &ca,
        Instant::now(),
        SMALL_STREAM_WINDOW,
    );

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    // Un frame inválido con id recuperable abre el stream en el servidor; el
    // `error` de respuesta nunca se lee en el peer.
    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    let frame = raw_frame(
        r#"{"schema_version":"arl.control/v1","type":"config.nope","request_id":"b4-tipo"}"#,
    );
    write_frame(&mut peer.conn, stream, &frame);
    exchange(&mut peer, &mut server, 8).expect("bombeo del frame inválido");

    let reply = ControlMessage::Error(
        ErrorMsg::new("srv-b4", ErrorCode::Invalid, "relleno de prueba").expect("error válido"),
    );
    let mut accepted = 0usize;
    let mut full = false;
    for _ in 0..512 {
        match server.send_control(session, &reply) {
            Ok(()) => accepted += 1,
            Err(crate::quinn::TransportError::QueueFull) => {
                full = true;
                break;
            }
            Err(error) => panic!("error inesperado antes de QueueFull: {error:?}"),
        }
    }
    assert!(
        full,
        "sin lectura del peer, la cola debe llenarse: {accepted} aceptados"
    );
    assert!(
        (1..=9).contains(&accepted),
        "QueueFull llega tras pocos frames acotados: {accepted}"
    );
}
