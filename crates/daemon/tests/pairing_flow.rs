//! Flujo completo de emparejamiento sobre QUIC en memoria: enrolar -> pendiente
//! -> aprobación local -> identidad persistente -> mTLS autorizado.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{exchange, roots, token, TestCa, TestLeaf, CLIENT_ADDR, SERVER_ADDR};
use openrelay_daemon::pairing::{self, Pairing};
use openrelay_transport::enrollment::{
    EnrollClient, EnrollReject, EnrollServer, EnrollmentEvent, EnrollmentToken,
};
use openrelay_transport::identity::{extract_spki_der, SpkiPin};
use openrelay_transport::quinn::{
    Capabilities, ClientEvent, RejectReason, ServerEvent, SessionClient, SessionLimits,
    SessionServer,
};
use serde_json::json;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("openrelay-pairing-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("directorio de prueba");
    dir.join("identities.json")
}

fn cleanup(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir_all(dir);
    }
}

fn drain_enroll_server(server: &mut EnrollServer) -> Vec<EnrollmentEvent> {
    std::iter::from_fn(|| server.next_event()).collect()
}

fn accepted_pin(events: &[EnrollmentEvent]) -> Option<SpkiPin> {
    events.iter().find_map(|event| match event {
        EnrollmentEvent::Accepted(pin) => Some(*pin),
        _ => None,
    })
}

/// Servidor de enrolamiento con ventana abierta y cliente con el pin correcto.
fn enroll_pair(ca: &TestCa, window: EnrollmentToken) -> (EnrollClient, EnrollServer, SpkiPin) {
    let leaf = ca.leaf("openrelay-enroll-pairing", true, false);
    let mut server = EnrollServer::new(leaf.certified(ca)).expect("servidor enroll");
    server
        .open_window(window, Duration::from_secs(120), Instant::now())
        .expect("ventana");
    let client = EnrollClient::connect(
        roots(ca),
        leaf.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente enroll");
    (client, server, leaf.pin)
}

/// Enrolamiento completo de `android`: devuelve el pin que el servidor reporta.
fn enroll_client(
    android: &TestLeaf,
    window: &EnrollmentToken,
    server: &mut EnrollServer,
    client: &mut EnrollClient,
    pairing: &mut Pairing,
) -> Option<SpkiPin> {
    let spki = extract_spki_der(android.cert.der()).expect("SPKI del cliente");
    exchange(client, CLIENT_ADDR, server, SERVER_ADDR, 24).expect("handshake");
    client.send_token_async(window, spki).expect("token");
    exchange(client, CLIENT_ADDR, server, SERVER_ADDR, 8).expect("vuelo");
    let events = drain_enroll_server(server);
    for event in &events {
        pairing.observe(event);
    }
    accepted_pin(&events)
}

/// Cliente de sesión mTLS con `android` contra un `SessionServer` con `acl`.
fn session_pair(
    ca: &TestCa,
    daemon: &TestLeaf,
    android: &TestLeaf,
    acl: openrelay_transport::quinn::Acl,
) -> (SessionClient, SessionServer) {
    let server = SessionServer::new(
        daemon.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión");
    let client = SessionClient::connect(
        android.certified(ca),
        roots(ca),
        daemon.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");
    (client, server)
}

#[test]
fn a_wrong_server_pin_never_sends_token() {
    let ca = TestCa::new();
    let leaf = ca.leaf("openrelay-enroll-pairing", true, false);
    let impostor = ca.leaf("openrelay-impostor", true, false);
    let android = ca.leaf("openrelay-android", false, false);
    let mut server = EnrollServer::new(leaf.certified(&ca)).expect("servidor enroll");
    let window = token();
    server
        .open_window(window.clone(), Duration::from_secs(120), Instant::now())
        .expect("ventana");
    let mut client = EnrollClient::connect(
        roots(&ca),
        impostor.pin,
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente con pin equivocado");

    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
    assert!(!client.is_ready(), "pin erróneo: sin handshake listo");
    let spki = extract_spki_der(android.cert.der()).expect("SPKI");
    assert!(
        client.send_token_async(&window, spki).is_err(),
        "sin readiness no sale el token"
    );
    assert_eq!(server.attempts(), 0, "el token nunca llegó al servidor");
}

#[test]
fn b_expired_and_reused_tokens_are_rejected() {
    let ca = TestCa::new();
    let android = ca.leaf("openrelay-android", false, false);
    let spki = extract_spki_der(android.cert.der()).expect("SPKI");

    // Vencido: la ventana ya expiró al llegar el mensaje.
    let expired = token();
    let leaf = ca.leaf("openrelay-enroll-pairing", true, false);
    let mut server = EnrollServer::new(leaf.certified(&ca)).expect("servidor enroll");
    server
        .open_window(expired.clone(), Duration::from_nanos(1), Instant::now())
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
    client.send_token_async(&expired, spki).expect("token");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo");
    let events = drain_enroll_server(&mut server);
    assert!(accepted_pin(&events).is_none(), "vencido: sin aceptación");
    assert!(events
        .iter()
        .any(|event| matches!(event, EnrollmentEvent::Rejected(EnrollReject::Expired))));

    // Reutilizado: tras un Accepted la ventana se cierra. Un segundo
    // `enroll.request` con el mismo token, en la misma conexión, no se acepta.
    let once = token();
    let (mut client, mut server, _) = enroll_pair(&ca, once.clone());
    let mut scratch_pairing = Pairing::new();
    assert!(enroll_client(
        &android,
        &once,
        &mut server,
        &mut client,
        &mut scratch_pairing
    )
    .is_some());
    client
        .send_token_async(&once, spki)
        .expect("token reutilizado");
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo 2");
    let replay = drain_enroll_server(&mut server);
    assert!(
        accepted_pin(&replay).is_none(),
        "reutilizado: sin segunda aceptación"
    );
    assert!(replay.iter().any(|event| matches!(
        event,
        EnrollmentEvent::Rejected(EnrollReject::NoWindow | EnrollReject::Reused)
    )));
}

#[test]
fn c_enrolled_identity_stays_pending_and_writes_nothing() {
    let ca = TestCa::new();
    let android = ca.leaf("openrelay-android", false, false);
    let window = token();
    let (mut client, mut server, _) = enroll_pair(&ca, window.clone());
    let path = scratch("pending");

    let mut pairing = Pairing::new();
    let pin = enroll_client(&android, &window, &mut server, &mut client, &mut pairing)
        .expect("aceptado con pin");
    assert_eq!(
        pin, android.pin,
        "el pin es el SPKI declarado por el cliente"
    );
    assert_eq!(pairing.pending(), &[pin], "queda pendiente");
    assert!(!path.exists(), "nada se persiste antes de approve");
    cleanup(&path);
}

#[test]
fn d_approval_persists_identity_and_authorizes_mtls() {
    let ca = TestCa::new();
    let daemon = ca.leaf("openrelay-daemon", true, false);
    let android = ca.leaf("openrelay-android", false, false);
    let window = token();
    let (mut enroll_client_side, mut enroll_server, _) = enroll_pair(&ca, window.clone());
    let path = scratch("approve");
    let caps = Capabilities::none().with_mic();

    // Antes de aprobar: mTLS con la misma identidad es rechazado.
    let (mut before_client, mut before_server) = session_pair(
        &ca,
        &daemon,
        &android,
        openrelay_transport::quinn::Acl::new(),
    );
    exchange(
        &mut before_client,
        CLIENT_ADDR,
        &mut before_server,
        SERVER_ADDR,
        24,
    )
    .expect("handshake sin ACL");
    assert!(!before_client.is_authorized());
    let rejected = std::iter::from_fn(|| before_server.next_event()).any(|event| {
        matches!(
            event,
            ServerEvent::Rejected {
                reason: RejectReason::NoAclEntry
            }
        )
    });
    assert!(rejected, "antes de approve: NoAclEntry");

    // Enrolar y aprobar localmente.
    let mut pairing = Pairing::new();
    let pin = enroll_client(
        &android,
        &window,
        &mut enroll_server,
        &mut enroll_client_side,
        &mut pairing,
    )
    .expect("aceptado");
    pairing.approve(pin, caps, &path).expect("approve");
    assert!(path.exists(), "approve escribe el archivo");
    assert!(pairing.pending().is_empty(), "approve retira el pendiente");

    // Recarga desde disco y sesión mTLS autorizada.
    let acl = pairing::load(&path).expect("load");
    assert_eq!(acl.capabilities(&pin), Some(caps), "ACL con ese pin");
    let (mut client, mut server) = session_pair(&ca, &daemon, &android, acl);
    exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake mTLS");
    assert!(client.is_authorized(), "cliente autorizado tras approve");
    let authorized = std::iter::from_fn(|| server.next_event()).any(|event| {
        matches!(
            event,
            ServerEvent::Authorized { identity, .. } if identity == pin
        )
    });
    assert!(authorized, "el servidor autoriza esa identidad");
    let client_authorized = std::iter::from_fn(|| client.next_event())
        .any(|event| matches!(event, ClientEvent::Authorized { .. }));
    assert!(client_authorized, "el cliente recibe Authorized");
    cleanup(&path);
}

#[test]
fn e_audio_and_control_on_enroll_surface_are_rejected() {
    for surface in ["audio", "control"] {
        let ca = TestCa::new();
        let window = token();
        let (mut client, mut server, _) = enroll_pair(&ca, window);
        exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 24).expect("handshake");
        client
            .send_unchecked_frame(&json!({"type": surface, "version": 1}))
            .expect("frame");
        exchange(&mut client, CLIENT_ADDR, &mut server, SERVER_ADDR, 8).expect("vuelo");
        let events = drain_enroll_server(&mut server);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EnrollmentEvent::ForbiddenSurface(_))),
            "{surface} prohibido en enrolamiento"
        );
        assert!(accepted_pin(&events).is_none());
    }
}

#[test]
fn qr_payload_has_the_documented_shape() {
    let ca = TestCa::new();
    let leaf = ca.leaf("openrelay-enroll-pairing", true, false);
    let window = token();
    let addr = SERVER_ADDR;
    let qr = Pairing::qr_payload(leaf.pin, addr, &window);
    assert_eq!(
        qr,
        format!(
            "openrelay://pair?v=1&h={addr}&pin={}&t={}",
            leaf.pin.to_hex(),
            window.to_hex()
        )
    );
}
