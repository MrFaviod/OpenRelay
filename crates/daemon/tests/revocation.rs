//! Tickets cortos y revocación sobre QUIC en memoria: capacidades, tickets
//! (vencido, reutilizado, otra sesión, otra identidad), revocación con sesión
//! viva y auditoría sin material secreto.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{exchange, roots, TestCa, CLIENT_ADDR, SERVER_ADDR};
use openrelay_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};
use openrelay_daemon::acl::{self, TicketError, Tickets};
use openrelay_daemon::pairing;
use openrelay_transport::identity::SpkiPin;
use openrelay_transport::quinn::{
    Acl, Capabilities, ClientEvent, RejectReason, ServerEvent, SessionClient, SessionId,
    SessionLimits, SessionServer,
};

const TTL: Duration = Duration::from_secs(30);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrelay-revocation-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("directorio de prueba");
    dir.join("identities.json")
}

fn frame(seq: u32) -> AudioFrame {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample = (index as i16) - 120 + seq as i16;
    }
    AudioFrame::new(1, seq, Flags::NONE, samples).expect("frame v1")
}

fn client_for(android: &common::TestLeaf, daemon: &common::TestLeaf, ca: &TestCa) -> SessionClient {
    SessionClient::connect(
        android.certified(ca),
        roots(ca),
        daemon.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión")
}

fn server_for(daemon: &common::TestLeaf, ca: &TestCa, acl: Acl) -> SessionServer {
    SessionServer::new(
        daemon.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión")
}

/// Drena eventos y devuelve la sesión autorizada más reciente, si la hay.
fn authorized_session(server: &mut SessionServer) -> Option<SessionId> {
    std::iter::from_fn(|| server.next_event()).find_map(|event| match event {
        ServerEvent::Authorized { session, .. } => Some(session),
        _ => None,
    })
}

fn acl_with(pin: SpkiPin, caps: Capabilities) -> Acl {
    let mut acl = Acl::new();
    acl.allow(pin, caps);
    acl
}

#[test]
fn a_capability_not_granted_blocks_audio() {
    let ca = TestCa::new();
    let daemon = ca.leaf("openrelay-daemon", true, false);
    let android = ca.leaf("openrelay-android", false, false);

    // Sin `mic`: el audio enviado no llega a la cola de recepción.
    let mut server = server_for(
        &daemon,
        &ca,
        acl_with(android.pin, Capabilities::none().with_control_read()),
    );
    let mut client = client_for(&android, &daemon, &ca);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    let session = authorized_session(&mut server).expect("sesión autorizada");
    client.send_audio(&frame(1)).expect("envío del cliente");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo");
    assert!(server.recv_audio(session).is_none(), "sin mic: no llega");

    // Control positivo: con `mic` el mismo envío sí llega.
    let mut server = server_for(
        &daemon,
        &ca,
        acl_with(android.pin, Capabilities::none().with_mic()),
    );
    let mut client = client_for(&android, &daemon, &ca);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    let session = authorized_session(&mut server).expect("sesión autorizada");
    client.send_audio(&frame(2)).expect("envío del cliente");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo");
    assert_eq!(
        server.recv_audio(session).map(|f| f.samples()[0]),
        Some(frame(2).samples()[0]),
        "con mic: llega"
    );
}

#[test]
fn b_tickets_are_single_use_bound_and_short_lived() {
    let ca = TestCa::new();
    let daemon = ca.leaf("openrelay-daemon", true, false);
    let android = ca.leaf("openrelay-android", false, false);
    let other = ca.leaf("openrelay-other", false, false);
    let mut server = server_for(
        &daemon,
        &ca,
        acl_with(android.pin, Capabilities::none().with_mic()),
    );

    // Dos sesiones reales del mismo servidor: primera y segunda conexión.
    let mut first = client_for(&android, &daemon, &ca);
    exchange(&mut first, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake 1");
    let s1 = authorized_session(&mut server).expect("sesión 1");
    drop(first);
    let mut second = client_for(&android, &daemon, &ca);
    exchange(&mut second, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake 2");
    let s2 = authorized_session(&mut server).expect("sesión 2");
    assert_ne!(s1, s2, "dos sesiones distintas");

    let now = Instant::now();
    let mut tickets = Tickets::new();

    // Válido: canjea una vez.
    let t = tickets.issue(android.pin, s1, TTL, now);
    assert_eq!(tickets.redeem(&t, &android.pin, s1, now), Ok(()));
    assert_eq!(
        tickets.redeem(&t, &android.pin, s1, now),
        Err(TicketError::Unknown),
        "reutilizado"
    );

    // Vencido.
    let t = tickets.issue(android.pin, s1, TTL, now);
    assert_eq!(
        tickets.redeem(&t, &android.pin, s1, now + TTL + Duration::from_secs(1)),
        Err(TicketError::Expired)
    );

    // Otra sesión: rechazado y consumido.
    let t = tickets.issue(android.pin, s1, TTL, now);
    assert_eq!(
        tickets.redeem(&t, &android.pin, s2, now),
        Err(TicketError::WrongSession)
    );
    assert_eq!(
        tickets.redeem(&t, &android.pin, s1, now),
        Err(TicketError::Unknown),
        "tras el rechazo ya no vale"
    );

    // Otra identidad.
    let t = tickets.issue(android.pin, s1, TTL, now);
    assert_eq!(
        tickets.redeem(&t, &other.pin, s1, now),
        Err(TicketError::WrongIdentity)
    );
}

#[test]
fn c_revocation_closes_live_session_and_denies_reconnect() {
    let ca = TestCa::new();
    let daemon = ca.leaf("openrelay-daemon", true, false);
    let android = ca.leaf("openrelay-android", false, false);
    let path = scratch("revoke");
    let doc = serde_json::json!({
        "version": 1,
        "identities": [{"spki": android.pin.to_hex(), "mic": true,
            "control_read": false, "control_write": false, "return_audio": false}]
    });
    fs::write(&path, doc.to_string()).expect("archivo de identidades");
    let acl = pairing::load(&path).expect("load");
    let mut server = server_for(&daemon, &ca, acl);

    let mut client = client_for(&android, &daemon, &ca);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    let session = authorized_session(&mut server).expect("sesión viva");
    assert!(client.is_authorized());

    let mut tickets = Tickets::new();
    let now = Instant::now();
    let ticket = tickets.issue(android.pin, session, TTL, now);

    acl::revoke(&path, &mut tickets, &android.pin).expect("revoke en archivo");
    server.revoke(&android.pin, now);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo de cierre");

    let closed_server = std::iter::from_fn(|| server.next_event())
        .any(|event| matches!(event, ServerEvent::Closed { session: s } if s == session));
    assert!(closed_server, "el servidor emite Closed");
    let closed_client = std::iter::from_fn(|| client.next_event())
        .any(|event| matches!(event, ClientEvent::Closed));
    assert!(closed_client, "el cliente ve la conexión cerrada");
    assert!(!client.is_authorized(), "sesión ya no autorizada");

    assert_eq!(
        tickets.redeem(&ticket, &android.pin, session, now),
        Err(TicketError::Unknown),
        "tickets de la identidad invalidados"
    );

    // Reconexión: el handshake llega pero la autorización falla.
    drop(client);
    let mut again = client_for(&android, &daemon, &ca);
    exchange(&mut again, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake nuevo");
    assert!(!again.is_authorized(), "reconexión no autorizada");
    let no_entry = std::iter::from_fn(|| server.next_event()).any(|event| {
        matches!(
            event,
            ServerEvent::Rejected {
                reason: RejectReason::NoAclEntry
            }
        )
    });
    assert!(no_entry, "reconexión: NoAclEntry");

    // Tras recargar el archivo sigue denegado.
    let reloaded = pairing::load(&path).expect("reload");
    assert_eq!(reloaded.capabilities(&android.pin), None);
    let _ = fs::remove_dir_all(path.parent().expect("dir"));
}

#[test]
fn d_audit_never_contains_the_ticket() {
    let ca = TestCa::new();
    let android = ca.leaf("openrelay-android", false, false);
    let mut tickets = Tickets::new();
    let now = Instant::now();
    let session = session_from_pair_for_audit(&ca, &android);
    let ticket = tickets.issue(android.pin, session, TTL, now);
    let _ = tickets.redeem(&ticket, &android.pin, session, now);
    tickets.revoke_identity(&android.pin);

    let lines = tickets.audit_lines().join("\n");
    let ticket_hex = ticket.iter().fold(String::new(), |mut acc, b| {
        acc.push_str(&format!("{b:02x}"));
        acc
    });
    assert!(!lines.contains(&ticket_hex), "auditoría sin ticket");
    assert!(
        !lines.contains(&android.pin.to_hex()),
        "auditoría sin pin completo"
    );
    assert!(!lines.is_empty(), "hay auditoría");
}

/// Sesión autorizada real para obtener un `SessionId` opaco.
fn session_from_pair_for_audit(ca: &TestCa, android: &common::TestLeaf) -> SessionId {
    let daemon = ca.leaf("openrelay-daemon", true, false);
    let mut server = server_for(&daemon, ca, acl_with(android.pin, Capabilities::none()));
    let mut client = client_for(android, &daemon, ca);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    authorized_session(&mut server).expect("sesión")
}
