//! Regresiones B de ciclo de vida y envío público.
//!
//! Dos mecanismos, sin mocks: peer QUIC crudo con handshake mTLS real, ALPN
//! `openrelay-session/1` y pin SPKI, igual que el cliente de `quinn_regressions`.
//!
//! 1. **Pérdida remota de la conexión** (`Event::ConnectionLost`): tras el
//!    evento no debe quedar estado autorizado ni residuo observable — ni
//!    identidad, ni capacidades, ni colas de audio/control, y las operaciones
//!    públicas deben rechazar con `Unauthorized` o `Closed`.
//! 2. **Envío público bloqueado a medio frame**: `send_control` escribe en el
//!    stream que abre el servidor. Con una ventana de recepción real pequeña,
//!    la escritura acepta un prefijo y devuelve `Err`. El contrato admite `Ok`
//!    como aceptación **íntegra para entrega asíncrona** (cola acotada u offset
//!    interno), nunca como entrega síncrona completa; un `Err`, en cambio, debe
//!    dejar **cero** bytes del frame, porque `Result` no transporta offset y un
//!    prefijo ya escrito es irreversible.
//!
//! Este corte **no** modifica producción: los `assert!` describen el contrato
//! y un fallo es un hallazgo, no una licencia para tocar `quinn.rs`.

use std::time::Instant;

use bytes::Bytes;
use openrelay_audio_core::pcm16::SILENCE;
use openrelay_audio_core::{AudioFrame, Flags};
use openrelay_proto::control::{ErrorCode, ErrorMsg};
use openrelay_proto::wire::{self, ControlMessage};
use quinn_proto::{Dir, VarInt};

use super::super::{authorized_session, exchange, roots, TestCa, TestLeaf};
use super::{raw_frame, raw_peer_with_stream_window, read_available, write_frame, RawPeer};
use crate::quinn::{Acl, Capabilities, SessionLimits, SessionServer, TransportError};

/// Ventana de recepción real del peer, en bytes: menor que cualquier frame de
/// control útil, así que la escritura del servidor se trunca de verdad.
const TINY_STREAM_WINDOW: u32 = 64;
/// Ventana amplia de control: cabe cualquier frame de este módulo.
const WIDE_STREAM_WINDOW: u32 = 65_536;

/// ACL con todas las capacidades de v1, para que la capa de capacidades no
/// enmascare un fallo de ciclo de vida con `Capability(...)`.
fn full_capabilities() -> Capabilities {
    Capabilities::none()
        .with_mic()
        .with_control_read()
        .with_control_write()
        .with_return_audio()
}

fn server_with_full_caps(
    server_leaf: &TestLeaf,
    client_leaf: &TestLeaf,
    ca: &TestCa,
) -> SessionServer {
    let mut acl = Acl::new();
    acl.allow(client_leaf.pin, full_capabilities());
    SessionServer::new(
        server_leaf.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión con ACL completa")
}

fn ping(request_id: &str) -> ControlMessage {
    ControlMessage::Ping {
        request_id: request_id.to_string(),
    }
}

fn ping_frame(request_id: &str) -> Vec<u8> {
    wire::encode_message(&ping(request_id)).expect("ping codificable")
}

fn audio_frame(seq: u32) -> AudioFrame {
    AudioFrame::new(1, seq, Flags::SILENCE, SILENCE).expect("frame de audio de test")
}

fn audio_datagram(seq: u32) -> Vec<u8> {
    audio_frame(seq).encode().to_vec()
}

/// Devuelve `true` si el error público es el rechazo que el contrato admite
/// para una sesión cerrada.
fn is_closed_rejection(error: &TransportError) -> bool {
    matches!(error, TransportError::Unauthorized | TransportError::Closed)
}

/// Residuo interno observable de una sesión: `(audio, control, rx, tx, closed)`.
///
/// El módulo de test es descendiente del módulo `quinn`, así que puede leer los
/// campos privados de `Slot` sin abrir API nueva ni tocar producción. Con
/// `None` la sesión ya no existe en el servidor.
fn slot_residue(
    server: &SessionServer,
    session: crate::quinn::SessionId,
) -> Option<(usize, usize, usize, usize, bool)> {
    server.slots.get(&session).map(|slot| {
        (
            slot.audio.len(),
            slot.control.len(),
            slot.control_rx.partial.len(),
            slot.control_tx.per_stream.len(),
            slot.closed,
        )
    })
}

/// Frame de control grande (más de 64 B) pero bajo `max_control_frame`.
fn big_control(request_id: &str) -> ControlMessage {
    let message = "x".repeat(200);
    ControlMessage::Error(
        ErrorMsg::new(request_id, ErrorCode::Internal, &message).expect("error grande construible"),
    )
}

/// Acepta el stream que el servidor abre para `send_control` y devuelve su id.
fn accept_server_stream(peer: &mut RawPeer) -> quinn_proto::StreamId {
    peer.conn
        .streams()
        .accept(Dir::Bi)
        .expect("el servidor abre su stream de control")
}

// ---------------------------------------------------------------------------
// Mecanismo 1: pérdida remota de la conexión.
// ---------------------------------------------------------------------------

/// B-lifecycle 1: `ConnectionLost` remoto con residuo pendiente (respuesta
/// `error` bloqueada por ventana, un `ping` y un frame de audio aún en cola).
///
/// Precondición no vacua: antes del cierre se comprueba identidad, capacidades
/// y que la entrega de control/audio funciona (se consume un elemento de cada
/// cola, dejando otro). Después del cierre se exige que no quede nada de eso y
/// que las operaciones públicas rechacen con `Unauthorized` o `Closed`.
///
/// Las observaciones se acumulan en vez de abortar en la primera: un solo
/// `assert!` por propiedad dejaría el resto sin medir tras el primer fallo.
#[test]
fn b_lifecycle_connection_lost_clears_state_and_residue() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10b-life", true);
    let client_leaf = ca.leaf("openrelay-client-t10b-life", false);
    let mut server = server_with_full_caps(&server_leaf, &client_leaf, &ca);
    let mut peer = raw_peer_with_stream_window(
        &client_leaf,
        &server_leaf,
        &ca,
        Instant::now(),
        TINY_STREAM_WINDOW,
    );

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    assert!(peer.is_authorized(), "peer con handshake completo");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    // --- Precondición no vacua: estado autorizado visible antes del cierre.
    assert_eq!(
        server.identity(session),
        Some(client_leaf.pin),
        "precondición: identidad presente antes del cierre"
    );
    assert_eq!(
        server.capabilities(session),
        Some(full_capabilities()),
        "precondición: capacidades presentes antes del cierre"
    );

    // Residuo de control: dos pings; se consume uno para demostrar que la ruta
    // de entrega funciona, el otro queda en cola cuando llega el cierre.
    let stream = peer
        .conn
        .streams()
        .open(Dir::Bi)
        .expect("stream bidireccional");
    write_frame(&mut peer.conn, stream, &ping_frame("life-ping-1"));
    write_frame(&mut peer.conn, stream, &ping_frame("life-ping-2"));
    // Residuo de audio: dos frames; se consume uno.
    for seq in [1u32, 2] {
        peer.conn
            .datagrams()
            .send(Bytes::copy_from_slice(&audio_datagram(seq)), true)
            .expect("datagrama de audio aceptado");
    }
    exchange(&mut peer, &mut server, 16).expect("bombeo del residuo");

    assert!(
        server.recv_control(session).is_some(),
        "precondición: la entrega de control funciona antes del cierre"
    );
    assert!(
        server.recv_audio(session).is_some(),
        "precondición: la entrega de audio funciona antes del cierre"
    );

    // Residuo de respuesta: el frame inválido produce un `error` de más bytes
    // que la ventana del peer, así que QUIC acepta sólo un prefijo y el resto
    // queda pendiente en `control_tx` cuando llega el cierre.
    let pending = super::expected_invalid_error("life-pend");
    assert!(
        pending.len() as u32 > TINY_STREAM_WINDOW,
        "el error pendiente excede la ventana del peer ({} > {})",
        pending.len(),
        TINY_STREAM_WINDOW
    );
    write_frame(
        &mut peer.conn,
        stream,
        &raw_frame(
            r#"{"schema_version":"openrelay.control/v1","type":"config.nope","request_id":"life-pend"}"#,
        ),
    );
    exchange(&mut peer, &mut server, 8).expect("bombeo del frame inválido");
    assert!(
        peer.application_close.is_none(),
        "precondición: un frame recuperable no cierra la conexión"
    );

    // --- Cierre remoto: el peer cierra; el servidor observa ConnectionLost.
    peer.conn.close(
        Instant::now(),
        VarInt::from_u32(0x104),
        Bytes::from_static(b"bye"),
    );
    let mut closed_events = 0usize;
    for _ in 0..64 {
        exchange(&mut peer, &mut server, 1).expect("bombeo del cierre");
        while let Some(event) = server.next_event() {
            if matches!(event, crate::quinn::ServerEvent::Closed { session: id } if id == session) {
                closed_events += 1;
            }
        }
        if closed_events > 0 {
            break;
        }
    }
    assert_eq!(
        closed_events, 1,
        "el servidor observa el cierre una sola vez"
    );

    // --- Observaciones independientes tras el evento.
    let mut violations: Vec<String> = Vec::new();

    if server.identity(session).is_some() {
        violations.push(format!(
            "identity(session) sigue en {:?} tras ConnectionLost",
            server.identity(session)
        ));
    }
    if server.capabilities(session).is_some() {
        violations.push(format!(
            "capabilities(session) sigue en {:?} tras ConnectionLost",
            server.capabilities(session)
        ));
    }
    if let Some(message) = server.recv_control(session) {
        violations.push(format!(
            "recv_control entrega residuo {message:?} tras ConnectionLost"
        ));
    }
    if let Some(frame) = server.recv_audio(session) {
        violations.push(format!(
            "recv_audio entrega residuo seq={} tras ConnectionLost",
            frame.seq()
        ));
    }
    match server.send_control(session, &ping("life-after")) {
        Ok(()) => violations.push("send_control aceptó un ping tras ConnectionLost".to_string()),
        Err(error) if is_closed_rejection(&error) => {}
        Err(error) => violations.push(format!(
            "send_control devuelve {error:?} en vez de Unauthorized/Closed"
        )),
    }
    match server.send_audio(session, &audio_frame(9)) {
        Ok(()) => violations.push("send_audio aceptó un frame tras ConnectionLost".to_string()),
        Err(error) if is_closed_rejection(&error) => {}
        Err(error) => violations.push(format!(
            "send_audio devuelve {error:?} en vez de Unauthorized/Closed"
        )),
    }

    // Colas internas del slot: TX (`control_tx`) y RX (`control`, `audio`,
    // `control_rx`) deben quedar vacías. Se leen como observación, no como
    // oráculo debilitado: un `None` (slot retirado) también es ausencia de
    // residuo y no compensa las violaciones públicas ya registradas.
    if let Some((audio, control, partial, pending_tx, closed)) = slot_residue(&server, session) {
        if audio > 0 {
            violations.push(format!("cola RX de audio con {audio} frame(s)"));
        }
        if control > 0 {
            violations.push(format!("cola RX de control con {control} mensaje(s)"));
        }
        if partial > 0 {
            violations.push(format!("{partial} prefijo(s) de control a medio llegar"));
        }
        if pending_tx > 0 {
            violations.push(format!("cola TX con {pending_tx} stream(s) de respuesta"));
        }
        if !closed {
            violations.push("slot no marcado closed tras ConnectionLost".to_string());
        }
    }

    assert!(
        violations.is_empty(),
        "estado residual tras ConnectionLost:\n- {}",
        violations.join("\n- ")
    );
}

// ---------------------------------------------------------------------------
// Mecanismo 2: envío público bloqueado a medio frame.
// ---------------------------------------------------------------------------

/// Control de causalidad: con ventana amplia `send_control` entrega el frame
/// íntegro, una sola vez y en orden, y la sesión sigue viva.
#[test]
fn b_lifecycle_control_public_send_wide_window_is_whole_and_ordered() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10b-wide", true);
    let client_leaf = ca.leaf("openrelay-client-t10b-wide", false);
    let mut server = server_with_full_caps(&server_leaf, &client_leaf, &ca);
    let mut peer = raw_peer_with_stream_window(
        &client_leaf,
        &server_leaf,
        &ca,
        Instant::now(),
        WIDE_STREAM_WINDOW,
    );

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let first = big_control("wide-1");
    let second = big_control("wide-2");
    let expected: Vec<u8> = [&first, &second]
        .iter()
        .flat_map(|message| wire::encode_message(message).expect("frame codificable"))
        .collect();

    server
        .send_control(session, &first)
        .expect("con ventana amplia la operación se acepta");
    server
        .send_control(session, &second)
        .expect("con ventana amplia la segunda operación se acepta");
    exchange(&mut peer, &mut server, 16).expect("bombeo del envío público");

    let stream = accept_server_stream(&mut peer);
    let received = read_available(&mut peer.conn, stream);
    assert_eq!(
        received, expected,
        "con ventana amplia los dos frames llegan íntegros, una vez y en orden"
    );

    exchange(&mut peer, &mut server, 4).expect("bombeo posterior");
    assert!(
        read_available(&mut peer.conn, stream).is_empty(),
        "sin duplicados tras el envío aceptado"
    );
    assert!(
        peer.application_close.is_none(),
        "un envío aceptado no cierra la sesión"
    );
}

/// B-lifecycle 2 (contrato): `send_control` devuelve `Result<(), ...>` y no
/// transporta offset, así que sólo admite dos desenlaces honestos:
///
/// - `Ok(())`: el transporte aceptó el frame **íntegro** para entrega
///   asíncrona (cola acotada u offset interno). No promete entrega síncrona
///   completa: hay que liberar crédito y bombear hasta verlo llegar.
/// - `Err(_)`: rechazo **antes** de aceptar, con cero bytes del frame en el
///   peer. Un prefijo ya escrito es irreversible desde la API pública y
///   reintentar el frame completo desalinea el framing.
///
/// La contrapresión de QUIC (ventana de 64 B real, frame de más bytes) no
/// legitima un término medio: escribir un prefijo y devolver `Err` es
/// exactamente lo que el contrato prohíbe.
#[test]
fn b_lifecycle_public_send_blocked_is_whole_or_cleanly_rejected() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("openrelay-server-t10b-prefix", true);
    let client_leaf = ca.leaf("openrelay-client-t10b-prefix", false);
    let mut server = server_with_full_caps(&server_leaf, &client_leaf, &ca);
    let mut peer = raw_peer_with_stream_window(
        &client_leaf,
        &server_leaf,
        &ca,
        Instant::now(),
        TINY_STREAM_WINDOW,
    );

    exchange(&mut peer, &mut server, 24).expect("handshake mTLS");
    let session = authorized_session(&mut server).expect("sesión autorizada por la ACL");

    let first_message = big_control("prefix-1");
    let first_frame = wire::encode_message(&first_message).expect("frame codificable");
    assert!(
        first_frame.len() as u32 > TINY_STREAM_WINDOW,
        "el frame debe exceder la ventana real del peer ({} > {})",
        first_frame.len(),
        TINY_STREAM_WINDOW
    );

    let mut violations: Vec<String> = Vec::new();
    let mut limits: Vec<String> = Vec::new();

    // --- Operación 1: el frame excede la ventana real del peer.
    let first = server.send_control(session, &first_message);
    exchange(&mut peer, &mut server, 8).expect("bombeo del envío bloqueado");
    let stream = peer.conn.streams().accept(Dir::Bi);
    let mut received: Vec<u8> = match stream {
        Some(stream) => read_available(&mut peer.conn, stream),
        None => Vec::new(),
    };

    match &first {
        Err(error) => {
            // Rechazo limpio: no puede haber ni un byte del frame en el peer.
            if !received.is_empty() {
                violations.push(format!(
                    "send_control devolvió {error:?} tras escribir {} B de {} (prefijo estricto: {}): rechazo con prefijo irreversible y sin offset público",
                    received.len(),
                    first_frame.len(),
                    first_frame.starts_with(&received)
                ));
            } else {
                limits.push(format!(
                    "la operación 1 se rechazó en limpio con {error:?} bajo ventana de {TINY_STREAM_WINDOW} B"
                ));
            }
        }
        Ok(()) => {
            // Aceptado para entrega asíncrona: el frame debe llegar íntegro y
            // una sola vez al liberar crédito.
            let mut complete = std::mem::take(&mut received);
            if let Some(stream) = stream {
                for _ in 0..64 {
                    if complete.len() >= first_frame.len() {
                        break;
                    }
                    exchange(&mut peer, &mut server, 1).expect("bombeo del frame aceptado");
                    complete.extend(read_available(&mut peer.conn, stream));
                }
            }
            if complete.len() < first_frame.len() {
                violations.push(format!(
                    "send_control aceptó el frame pero sólo llegaron {} de {} B",
                    complete.len(),
                    first_frame.len()
                ));
            } else if complete != first_frame {
                violations.push(format!(
                    "el peer recibió {} B que no son el frame aceptado de {} B",
                    complete.len(),
                    first_frame.len()
                ));
            }
            received = complete;
        }
    }

    // --- Operación 2: otro frame con otro `request_id`. No es un reintento del
    // primero: son dos operaciones distintas y el framing debe seguir alineado.
    let second_message = big_control("prefix-2");
    let second_frame = wire::encode_message(&second_message).expect("frame codificable");
    let second = server.send_control(session, &second_message);

    let mut after: Vec<u8> = Vec::new();
    if let Some(stream) = stream {
        for _ in 0..32 {
            exchange(&mut peer, &mut server, 1).expect("bombeo de la segunda operación");
            after.extend(read_available(&mut peer.conn, stream));
            if after.len() >= second_frame.len() {
                break;
            }
        }
    }

    match &second {
        Err(error) => {
            if !after.is_empty() {
                violations.push(format!(
                    "la segunda operación devolvió {error:?} tras escribir {} B: prefijo irreversible",
                    after.len()
                ));
            } else if first.is_ok() {
                // La primera operación demostró que el stream entrega; con el
                // crédito ya liberado, un rechazo limpio deja la API sin salida
                // para el siguiente frame.
                violations.push(format!(
                    "tras aceptar la operación 1, la operación 2 se rechazó en limpio con {error:?} pese al crédito liberado"
                ));
            } else {
                limits.push(format!(
                    "la operación 2 también se rechazó en limpio con {error:?}"
                ));
            }
        }
        Ok(()) => {
            // Aceptada: íntegra, única y en orden tras la primera.
            let mut complete = std::mem::take(&mut after);
            if let Some(stream) = stream {
                for _ in 0..64 {
                    if complete.len() >= second_frame.len() {
                        break;
                    }
                    exchange(&mut peer, &mut server, 1).expect("bombeo de la segunda aceptada");
                    complete.extend(read_available(&mut peer.conn, stream));
                }
            }
            if complete.len() < second_frame.len() {
                violations.push(format!(
                    "la segunda operación se aceptó pero sólo llegaron {} de {} B",
                    complete.len(),
                    second_frame.len()
                ));
            } else if complete != second_frame {
                violations.push(format!(
                    "el stream no continúa con el segundo frame íntegro: {} B recibidos",
                    complete.len()
                ));
            }
            if !received.is_empty() && first.is_err() {
                violations.push(
                    "la segunda operación entregó tras un rechazo que dejó bytes de la primera"
                        .to_string(),
                );
            }
        }
    }

    // --- Rondas extra: ni duplicados ni frames no pedidos, y sesión viva.
    if let Some(stream) = stream {
        exchange(&mut peer, &mut server, 4).expect("bombeo posterior");
        let extra = read_available(&mut peer.conn, stream);
        if !extra.is_empty() {
            violations.push(format!(
                "{} B extra tras las dos operaciones: duplicado o frame no solicitado",
                extra.len()
            ));
        }
    }
    if peer.application_close.is_some() {
        violations.push("la contrapresión cerró la conexión".to_string());
    }

    // Límite, no violación: un rechazo en limpio es un desenlace admitido por
    // el contrato. Se reporta siempre, y sólo se adjunta al fallo si además hay
    // violaciones, para no convertir una limitación en un falso RED.
    for limit in &limits {
        eprintln!("límite del envío público: {limit}");
    }
    assert!(
        violations.is_empty(),
        "envío público bloqueado:\n- {}\n- límites: {}\n- bytes tras la operación 1: {}; bytes tras la operación 2: {}",
        violations.join("\n- "),
        if limits.is_empty() {
            "ninguno".to_string()
        } else {
            limits.join("; ")
        },
        received.len(),
        after.len()
    );
}
