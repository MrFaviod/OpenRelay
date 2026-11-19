//! Regresiones D sobre `Connection` QUIC real: fragmentación del framing de
//! control, `Blocked`, aislamiento entre streams, reset y FIN.
//!
//! Módulo nieto de `quinn` (hijo de `quinn_regressions`): reutiliza sus
//! fixtures privadas (CA local, hojas ECDSA en memoria, ACL, enlace en memoria)
//! y observa los buffers internos **por `StreamId`** para comprobar que el
//! prefijo parcial se conserva byte a byte y nunca se mezcla entre streams.
//!
//! Todos los casos hablan QUIC real mTLS+ACL: el peer escribe bytes crudos en
//! streams bidireccionales de su `Connection` y el receptor es el
//! `SessionServer` real. No hay `sleep`: el avance es el bombeo del enlace en
//! memoria, así que un `Blocked` es el `Blocked` real de `quinn`.
//!
//! Contrato fijado (docs/protocol.md §3): un prefijo `u32 BE` a medio llegar no
//! entrega ni cierra; los bytes parciales viven en el buffer de su `StreamId`;
//! reset a mitad de frame y FIN con frame truncado cierran `0x104` sin dejar
//! estado entregable.

use super::*;
use crate::quinn::{SessionId, SessionServer};
use openrelay_proto::wire;
use quinn_proto::{StreamId, VarInt};

// ---------------------------------------------------------------------------
// Utilidades de escritura cruda sobre el peer: permiten partir un frame en los
// tramos exactos que el caso necesita, sin pasar por `SessionClient`.
// ---------------------------------------------------------------------------

/// Abre un stream bidireccional en el peer crudo.
fn open_stream(conn: &mut Connection) -> StreamId {
    conn.streams()
        .open(Dir::Bi)
        .expect("stream bidireccional disponible")
}

/// Escribe `bytes` completos en `stream` (falla si no hay progreso).
fn write_raw(conn: &mut Connection, stream: StreamId, bytes: &[u8]) {
    let mut written = 0usize;
    while written < bytes.len() {
        match conn.send_stream(stream).write(&bytes[written..]) {
            Ok(0) => panic!("escritura bloqueada sin progreso"),
            Ok(count) => written += count,
            Err(error) => panic!("escritura cruda rechazada: {error:?}"),
        }
    }
    assert_eq!(written, bytes.len(), "tramo crudo completo en el stream");
}

/// Cierra el lado de escritura del peer (FIN) sin escribir más bytes.
fn finish_stream(conn: &mut Connection, stream: StreamId) {
    conn.send_stream(stream)
        .finish()
        .expect("FIN del stream de control");
}

/// Abandona el stream con `code` (RESET_STREAM).
fn reset_stream(conn: &mut Connection, stream: StreamId, code: u32) {
    conn.send_stream(stream)
        .reset(VarInt::from_u32(code))
        .expect("reset del stream de control");
}

/// Bytes parciales que el receptor retiene para `stream`, en orden.
fn held_bytes(server: &SessionServer, session: SessionId, stream: StreamId) -> Vec<u8> {
    server
        .slots
        .get(&session)
        .and_then(|slot| slot.control_rx.partial.get(&stream).cloned())
        .unwrap_or_default()
}

fn ping(request_id: &str) -> Vec<u8> {
    wire::encode_message(&ControlMessage::Ping {
        request_id: request_id.to_string(),
    })
    .expect("frame de control codificable")
}

fn pong(request_id: &str) -> Vec<u8> {
    wire::encode_message(&ControlMessage::Pong {
        request_id: request_id.to_string(),
    })
    .expect("frame de control codificable")
}

// ---------------------------------------------------------------------------
// D1: prefijo 1+2+1 y cuerpo en 2 fragmentos, con bombeo intermedio.
// ---------------------------------------------------------------------------

/// Caso D1: el prefijo `u32 BE` llega en tres tramos (1, 2 y 1 bytes) y el
/// cuerpo en dos, con bombeo real entre tramos. Antes del último byte no hay
/// entrega ni cierre; el buffer conserva exactamente los bytes recibidos; al
/// completarse el frame se entrega **una sola vez** con su `request_id`.
#[test]
fn d1_fragmented_prefix_and_body_wait_then_deliver_once() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d1", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d1", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = open_stream(&mut peer.conn);
    let frame = ping("d1-fragmentado");
    assert!(frame.len() > 8, "frame con prefijo y cuerpo separables");
    let (prefix, body) = frame.split_at(4);

    // 1 de 4 bytes del prefijo.
    write_raw(&mut peer.conn, stream, &prefix[..1]);
    exchange(&mut peer, &mut server, 4).expect("bombeo de 1 byte de prefijo");
    assert_eq!(
        peer.application_close, None,
        "sin cierre con 1/4 de prefijo"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "nada entregado con 1/4 de prefijo"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        prefix[..1].to_vec(),
        "buffer exacto tras 1 byte de prefijo"
    );
    assert!(
        server.identity(session).is_some(),
        "la sesión sigue autorizada a mitad de prefijo"
    );

    // 3 de 4 bytes del prefijo.
    write_raw(&mut peer.conn, stream, &prefix[1..3]);
    exchange(&mut peer, &mut server, 4).expect("bombeo de 2 bytes de prefijo");
    assert_eq!(
        peer.application_close, None,
        "sin cierre con 3/4 de prefijo"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        prefix[..3].to_vec(),
        "buffer exacto tras 3 bytes de prefijo"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "nada entregado con 3/4 de prefijo"
    );

    // Prefijo completo, cuerpo ausente: se conserva y no entrega.
    write_raw(&mut peer.conn, stream, &prefix[3..]);
    exchange(&mut peer, &mut server, 4).expect("bombeo del último byte de prefijo");
    assert_eq!(
        peer.application_close, None,
        "un prefijo completo sin cuerpo no cierra"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        prefix.to_vec(),
        "prefijo completo conservado tal cual"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "un prefijo completo sin cuerpo no entrega"
    );

    // Cuerpo en dos tramos.
    let split = body.len() / 2;
    write_raw(&mut peer.conn, stream, &body[..split]);
    exchange(&mut peer, &mut server, 4).expect("bombeo del primer tramo de cuerpo");
    assert_eq!(
        server.recv_control(session),
        None,
        "cuerpo a medias no entrega"
    );
    let mut expected = prefix.to_vec();
    expected.extend_from_slice(&body[..split]);
    assert_eq!(
        held_bytes(&server, session, stream),
        expected,
        "prefijo + primer tramo de cuerpo, sin huecos"
    );

    write_raw(&mut peer.conn, stream, &body[split..]);
    exchange(&mut peer, &mut server, 8).expect("bombeo del resto del cuerpo");
    assert_eq!(
        server.recv_control(session),
        Some(ControlMessage::Ping {
            request_id: "d1-fragmentado".to_string(),
        }),
        "entrega única con el request_id correcto"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "sin duplicado del frame"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        Vec::<u8>::new(),
        "buffer vaciado tras consumir el frame"
    );
    assert_eq!(
        peer.application_close, None,
        "fragmentar no provoca cierre espurio"
    );
    assert!(
        server.identity(session).is_some(),
        "la sesión sigue autorizada tras el frame fragmentado"
    );
}

// ---------------------------------------------------------------------------
// D2: dos streams bidireccionales permitidos, mensajes distintos intercalados.
// ---------------------------------------------------------------------------

/// Caso D2: dos streams `Bi` (el máximo de `SessionLimits::default`) con frames
/// distintos y escrituras intercaladas. Ningún stream completa el frame del
/// otro (no contaminación), ninguno se entrega dos veces, ambos llegan y cada
/// buffer queda acotado a sus propios bytes.
#[test]
fn d2_interleaved_streams_stay_isolated() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d2", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d2", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let left = open_stream(&mut peer.conn);
    let right = open_stream(&mut peer.conn);
    assert_ne!(left, right, "dos streams distintos");

    let a = ping("d2-izquierda");
    let b = pong("d2-derecha");

    // Prefijo parcial del izquierdo; luego el frame entero del derecho.
    write_raw(&mut peer.conn, left, &a[..2]);
    exchange(&mut peer, &mut server, 4).expect("bombeo del prefijo izquierdo");
    assert_eq!(
        server.recv_control(session),
        None,
        "2 bytes de prefijo no entregan nada"
    );
    write_raw(&mut peer.conn, right, &b);
    exchange(&mut peer, &mut server, 6).expect("bombeo del frame derecho");

    assert_eq!(
        server.recv_control(session),
        Some(ControlMessage::Pong {
            request_id: "d2-derecha".to_string(),
        }),
        "el stream derecho entrega su propio frame, completo y correcto"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "el prefijo izquierdo no se completa con bytes del otro stream"
    );
    assert_eq!(
        held_bytes(&server, session, left),
        a[..2].to_vec(),
        "buffer izquierdo acotado a sus 2 bytes"
    );
    assert_eq!(
        held_bytes(&server, session, right),
        Vec::<u8>::new(),
        "buffer derecho vaciado tras su frame"
    );

    // Resto del izquierdo: cada stream entrega lo suyo.
    write_raw(&mut peer.conn, left, &a[2..]);
    exchange(&mut peer, &mut server, 8).expect("bombeo del resto izquierdo");
    assert_eq!(
        server.recv_control(session),
        Some(ControlMessage::Ping {
            request_id: "d2-izquierda".to_string(),
        }),
        "el stream izquierdo entrega su mensaje, no el del vecino"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "sin duplicados de ninguno de los dos frames"
    );
    assert_eq!(
        held_bytes(&server, session, left),
        Vec::<u8>::new(),
        "buffer izquierdo vaciado"
    );
    assert_eq!(
        held_bytes(&server, session, right),
        Vec::<u8>::new(),
        "buffer derecho sigue vacío"
    );
    assert_eq!(
        peer.application_close, None,
        "sin cierre espurio por intercalar"
    );
    assert!(
        server.identity(session).is_some(),
        "la sesión sigue autorizada"
    );
}

// ---------------------------------------------------------------------------
// D3: reset parcial de un stream de control.
// ---------------------------------------------------------------------------

/// Caso D3: tras un frame válido en cola, el peer abre otro stream, deja un
/// prefijo parcial y lo resetea. El receptor cierra `0x104`, purga identidad,
/// capacidades, colas y buffers, y no entrega nada más (ni el frame ya en cola).
#[test]
fn d3_partial_reset_closes_0x104_and_purges_state() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d3", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d3", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let queued = open_stream(&mut peer.conn);
    let frame = ping("d3-antes-del-reset");
    write_raw(&mut peer.conn, queued, &frame);
    exchange(&mut peer, &mut server, 6).expect("bombeo del frame previo");
    assert_eq!(
        server
            .slots
            .get(&session)
            .expect("slot antes del reset")
            .control
            .len(),
        1,
        "el frame previo queda en cola sin leerse"
    );

    let partial = open_stream(&mut peer.conn);
    write_raw(&mut peer.conn, partial, &frame[..3]);
    exchange(&mut peer, &mut server, 4).expect("bombeo del prefijo parcial");
    assert_eq!(
        peer.application_close, None,
        "un prefijo parcial sin reset no cierra"
    );
    assert_eq!(
        held_bytes(&server, session, partial),
        frame[..3].to_vec(),
        "el prefijo parcial vive en su propio stream"
    );

    reset_stream(&mut peer.conn, partial, 0x0d);
    exchange(&mut peer, &mut server, 8).expect("bombeo del reset");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "reset a mitad de frame debe cerrar 0x104"
    );
    assert_eq!(server.identity(session), None, "identidad purgada");
    assert_eq!(server.capabilities(session), None, "capacidades purgadas");
    assert_eq!(
        server.recv_control(session),
        None,
        "ningún mensaje entregable tras el reset, ni el que estaba en cola"
    );
    assert_eq!(
        server.recv_audio(session),
        None,
        "ningún frame de audio residual"
    );
    if let Some(slot) = server.slots.get(&session) {
        assert!(slot.control.is_empty(), "cola de control purgada");
        assert!(slot.audio.is_empty(), "cola de audio purgada");
        assert!(
            slot.control_rx.partial.is_empty(),
            "buffers de prefijo por stream purgados"
        );
        assert!(
            slot.recv_streams.is_empty(),
            "streams de recepción olvidados"
        );
        assert!(!slot.authorized, "el slot ya no está autorizado");
    }
    assert!(
        !peer.is_authorized(),
        "el peer pierde la autorización al bombear el cierre"
    );
}

// ---------------------------------------------------------------------------
// D4: FIN con frame truncado, `Blocked` sin FIN y frames coalescidos.
// ---------------------------------------------------------------------------

/// Caso D4a: FIN con el prefijo a medias (2 de 4 bytes). No es un EOF limpio:
/// el receptor cierra `0x104` y no deja identidad ni bytes retenidos.
#[test]
fn d4_fin_with_partial_prefix_closes_0x104_without_residue() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d4a", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d4a", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = open_stream(&mut peer.conn);
    let frame = ping("d4-fin-prefijo");
    write_raw(&mut peer.conn, stream, &frame[..2]);
    exchange(&mut peer, &mut server, 4).expect("bombeo de 2 bytes de prefijo");
    assert_eq!(
        peer.application_close, None,
        "2 bytes de prefijo sin FIN siguen esperando"
    );

    finish_stream(&mut peer.conn, stream);
    exchange(&mut peer, &mut server, 12).expect("bombeo del FIN");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "FIN con prefijo a medias debe cerrar 0x104"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "sin entrega residual tras el FIN truncado"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        Vec::<u8>::new(),
        "sin bytes retenidos tras el FIN truncado"
    );
    assert_eq!(server.identity(session), None, "identidad purgada");
}

/// Caso D4b: FIN con el cuerpo truncado (prefijo completo y 3 bytes de cuerpo).
/// Cierra `0x104` sin entregar un frame incompleto ni conservar residuo.
#[test]
fn d4_fin_with_truncated_body_closes_0x104_without_residue() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d4b", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d4b", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = open_stream(&mut peer.conn);
    let frame = ping("d4-fin-cuerpo");
    write_raw(&mut peer.conn, stream, &frame[..7]);
    exchange(&mut peer, &mut server, 4).expect("bombeo de prefijo + 3 bytes");
    assert_eq!(
        server.recv_control(session),
        None,
        "prefijo + cuerpo parcial no entrega"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        frame[..7].to_vec(),
        "los 7 bytes retenidos son los enviados"
    );

    finish_stream(&mut peer.conn, stream);
    exchange(&mut peer, &mut server, 12).expect("bombeo del FIN");

    assert_eq!(
        peer.application_close,
        Some(CLOSE_CONTROL_FRAMING),
        "FIN con cuerpo truncado debe cerrar 0x104"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "sin entrega de un frame truncado"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        Vec::<u8>::new(),
        "sin prefijo retenido tras el cierre"
    );
    assert_eq!(server.capabilities(session), None, "capacidades purgadas");
}

/// Caso D4c: `Blocked` real sin FIN. Un prefijo a medio llegar y ningún cierre
/// del peer: el receptor no entrega, no cierra y conserva el prefijo a través
/// de bombeos sucesivos (el `Blocked` se resuelve al llegar el resto).
#[test]
fn d4_blocked_without_fin_keeps_waiting_and_preserves_prefix() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d4c", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d4c", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = open_stream(&mut peer.conn);
    let frame = ping("d4-blocked");
    write_raw(&mut peer.conn, stream, &frame[..6]);
    // Varios bombeos sin bytes nuevos: el receptor debe seguir esperando.
    exchange(&mut peer, &mut server, 12).expect("bombeo en espera");

    assert_eq!(
        peer.application_close, None,
        "sin FIN y sin violación no hay cierre"
    );
    assert_eq!(
        server.recv_control(session),
        None,
        "un frame incompleto no se entrega"
    );
    assert_eq!(
        held_bytes(&server, session, stream),
        frame[..6].to_vec(),
        "el prefijo y el cuerpo parcial sobreviven a los bombeos"
    );
    assert!(
        server.identity(session).is_some(),
        "la sesión sigue autorizada mientras espera"
    );
    assert!(peer.is_authorized(), "el peer sigue autorizado");

    // Al llegar el resto, el frame que estaba en `Blocked` se completa.
    write_raw(&mut peer.conn, stream, &frame[6..]);
    exchange(&mut peer, &mut server, 8).expect("bombeo del resto");
    assert_eq!(
        server.recv_control(session),
        Some(ControlMessage::Ping {
            request_id: "d4-blocked".to_string(),
        }),
        "el frame retenido se entrega entero al completarse"
    );
    assert_eq!(server.recv_control(session), None, "sin duplicado");
    assert_eq!(
        held_bytes(&server, session, stream),
        Vec::<u8>::new(),
        "buffer vaciado"
    );
}

/// Caso D4d: dos frames válidos del mismo stream coalescidos en una escritura y
/// cerrados con FIN. Se entregan en orden, sin cierre espurio y sin residuo.
#[test]
fn d4_two_coalesced_frames_deliver_in_order_without_close() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10d-d4d", true);
    let client_leaf = ca.leaf("openrelay-client-t10d-d4d", false);
    let mut server = server_for(&server_leaf, &client_leaf, &ca);
    let mut peer = RawPeer::connect(&client_leaf, &server_leaf, &ca, Instant::now());
    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(
        peer.is_authorized(),
        "peer crudo autorizado tras el handshake"
    );
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let stream = open_stream(&mut peer.conn);
    let first = ping("d4-coalescido-uno");
    let second = pong("d4-coalescido-dos");
    let mut both = Vec::with_capacity(first.len() + second.len());
    both.extend_from_slice(&first);
    both.extend_from_slice(&second);
    write_raw(&mut peer.conn, stream, &both);
    finish_stream(&mut peer.conn, stream);
    exchange(&mut peer, &mut server, 12).expect("bombeo de ambos frames");

    assert_eq!(
        server.recv_control(session),
        Some(ControlMessage::Ping {
            request_id: "d4-coalescido-uno".to_string(),
        }),
        "primer frame en orden"
    );
    assert_eq!(
        server.recv_control(session),
        Some(ControlMessage::Pong {
            request_id: "d4-coalescido-dos".to_string(),
        }),
        "segundo frame en orden"
    );
    assert_eq!(server.recv_control(session), None, "sin duplicados");
    assert_eq!(
        held_bytes(&server, session, stream),
        Vec::<u8>::new(),
        "sin bytes residuales tras el FIN limpio"
    );
    assert_eq!(
        peer.application_close, None,
        "un FIN tras frames íntegros no cierra"
    );
    assert!(
        server.identity(session).is_some(),
        "la sesión sigue autorizada tras el EOF limpio"
    );
}
