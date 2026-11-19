//! E2E F3 sobre QUIC en memoria: enrolar (QR -> token -> SPKI) -> pendiente ->
//! approve a archivo -> load ACL -> mTLS autorizado -> config.set con revisión
//! y ACK -> ticket emitido y canjeado -> revocación (sesión cerrada, ticket
//! inválido, reconexión NoAclEntry, archivo sin la identidad).

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{exchange, roots, token, TestCa, CLIENT_ADDR, SERVER_ADDR};
use openrelay_daemon::acl::{self, TicketError, Tickets};
use openrelay_daemon::pairing::{self, Pairing};
use openrelay_proto::control::{
    AuthorizedPeer, ConfigKey, ConfigState, ConfigValue, ControlSession, IdentityPolicy, PeerRole,
    RemoteAction,
};
use openrelay_proto::wire::{AckEffective, ConfigSetMsg, ControlMessage};
use openrelay_transport::enrollment::{EnrollClient, EnrollServer, EnrollmentEvent};
use openrelay_transport::identity::extract_spki_der;
use openrelay_transport::quinn::{
    Acl, Capabilities, ClientEvent, RejectReason, ServerEvent, SessionClient, SessionLimits,
    SessionServer,
};

const TTL: Duration = Duration::from_secs(30);

#[test]
fn enroll_approve_session_control_ticket_revoke() {
    let dir: PathBuf = std::env::temp_dir().join(format!("openrelay-f3-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("directorio de prueba");
    let path = dir.join("identities.json");

    let ca = TestCa::new();
    let daemon = ca.leaf("openrelay-daemon", true, false);
    let enroll_leaf = ca.leaf("openrelay-enroll-pairing", true, false);
    let android = ca.leaf("openrelay-android", false, false);
    let now = Instant::now();

    // 1) Enrolar: QR con pin y token -> token -> SPKI del cliente.
    let window = token();
    let qr = Pairing::qr_payload(enroll_leaf.pin, SERVER_ADDR, &window);
    assert!(
        qr.starts_with("openrelay://pair?v=1&"),
        "QR documentado: {qr}"
    );
    let mut enroll_server = EnrollServer::new(enroll_leaf.certified(&ca)).expect("enroll");
    enroll_server
        .open_window(window.clone(), Duration::from_secs(120), now)
        .expect("ventana");
    let mut enroll_client =
        EnrollClient::connect(roots(&ca), enroll_leaf.pin, now, SERVER_ADDR, "localhost")
            .expect("cliente enroll");
    let spki = extract_spki_der(android.cert.der()).expect("SPKI");
    exchange(
        &mut enroll_client,
        CLIENT_ADDR,
        &mut enroll_server,
        SERVER_ADDR,
        24,
    )
    .expect("handshake enroll");
    enroll_client.send_token(&window, spki).expect("token");
    exchange(
        &mut enroll_client,
        CLIENT_ADDR,
        &mut enroll_server,
        SERVER_ADDR,
        8,
    )
    .expect("vuelo");
    let events: Vec<EnrollmentEvent> = std::iter::from_fn(|| enroll_server.next_event()).collect();

    // 2) Pendiente: nada escrito antes de approve.
    let mut pairing = Pairing::new();
    for event in &events {
        pairing.observe(event);
    }
    assert_eq!(pairing.pending(), &[android.pin], "queda pendiente");
    assert!(!path.exists(), "sin approve no hay archivo");

    // 3) Approve a archivo (control read+write para el E2E de control).
    let caps = Capabilities::none()
        .with_mic()
        .with_control_read()
        .with_control_write();
    pairing.approve(android.pin, caps, &path).expect("approve");
    assert!(path.exists() && pairing.pending().is_empty());

    // 4) Load ACL y mTLS autorizado.
    let acl = pairing::load(&path).expect("load");
    assert_eq!(acl.capabilities(&android.pin), Some(caps));
    let mut server = SessionServer::new(
        daemon.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión");
    let mut client = SessionClient::connect(
        android.certified(&ca),
        roots(&ca),
        daemon.pin,
        SessionLimits::default(),
        now,
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    assert!(client.is_authorized(), "cliente autorizado tras approve");
    let session = std::iter::from_fn(|| server.next_event())
        .find_map(|event| match event {
            ServerEvent::Authorized {
                session, identity, ..
            } if identity == android.pin => Some(session),
            _ => None,
        })
        .expect("sesión autorizada con la identidad aprobada");

    // 5) config.set con revisión y ACK: el worker de control aplica el mensaje
    // recibido por recv_control y el ACK vuelve por send_control.
    let request = ControlMessage::ConfigSet(ConfigSetMsg {
        request_id: "f3-set-1".to_owned(),
        expected_revision: 0,
        key: ConfigKey::new("audio.mute").expect("clave"),
        value: ConfigValue::Bool(true),
    });
    client.send_control(&request).expect("config.set enviado");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo control");
    let received = server.recv_control(session).expect("config.set recibido");
    assert_eq!(received, request, "el servidor recibe el mismo mensaje");

    let peer = AuthorizedPeer::new(
        "android-f3",
        PeerRole::Android,
        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
    );
    let mut control = ControlSession::new(peer, ConfigState::new(), Box::new(IdentityPolicy));
    let outcome = control.handle(&received, 1_000);
    let reply = outcome.response.expect("respuesta de control");
    server.send_control(session, &reply).expect("ACK enviado");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo ACK");
    let ack = client.recv_control().expect("ACK recibido");
    match ack {
        ControlMessage::Ack(ack) => {
            assert_eq!(ack.request_id, "f3-set-1");
            assert!(ack.applied, "config.set aplicado");
            assert_eq!(ack.revision, 1, "revisión avanza a 1");
            assert!(matches!(ack.effective, AckEffective::Pair(_)));
        }
        other => panic!("se esperaba ack, llegó {other:?}"),
    }
    assert_eq!(control.revision(), 1);

    // 6) Ticket emitido y canjeado; un segundo ticket queda vivo para la revocación.
    let mut tickets = Tickets::new();
    let redeemed = tickets.issue(android.pin, session, TTL, now);
    assert_eq!(
        tickets.redeem(&redeemed, &android.pin, session, now),
        Ok(())
    );
    let live = tickets.issue(android.pin, session, TTL, now);

    // 7) Revocar: archivo sin la identidad, sesión cerrada, ticket inválido.
    acl::revoke(&path, &mut tickets, &android.pin).expect("revoke en archivo");
    server.revoke(&android.pin, now);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo de cierre");

    let closed_server = std::iter::from_fn(|| server.next_event())
        .any(|event| matches!(event, ServerEvent::Closed { session: s } if s == session));
    assert!(closed_server, "el servidor cierra la sesión");
    let closed_client = std::iter::from_fn(|| client.next_event())
        .any(|event| matches!(event, ClientEvent::Closed));
    assert!(closed_client, "el cliente ve la conexión cerrada");
    assert!(!client.is_authorized(), "sesión ya no autorizada");
    assert_eq!(
        tickets.redeem(&live, &android.pin, session, now),
        Err(TicketError::Unknown),
        "ticket vivo invalidado por revocación"
    );

    // 8) Reconexión: handshake llega, autorización falla con NoAclEntry.
    drop(client);
    let mut again = SessionClient::connect(
        android.certified(&ca),
        roots(&ca),
        daemon.pin,
        SessionLimits::default(),
        now,
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de reconexión");
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

    // 9) El archivo ya no tiene la identidad.
    let reloaded = pairing::load(&path).expect("reload");
    assert_eq!(reloaded.capabilities(&android.pin), None);
    assert!(Acl::is_empty(&reloaded), "ACL sin identidades tras revocar");

    let _ = fs::remove_dir_all(&dir);
}
