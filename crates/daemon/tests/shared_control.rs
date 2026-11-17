//! Control compartido sobre UDP/mTLS real.
//!
//! Dos clientes QUIC contra un `serve_with_control`. El intercambio pasa por
//! sockets `127.0.0.1`, no por el bombeo en memoria. La CA es la de
//! `common`, que sólo existe en tests.

#[path = "common/mod.rs"]
mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arl_audio_linux::ring;
use arl_daemon::control::{ControlRegistry, IdentityGrant, SharedControl};
use arl_daemon::serve_with_control;
use arl_proto::control::{ConfigKey, ConfigValue, ErrorCode, PeerRole, RemoteAction};
use arl_proto::wire::{self, ConfigGetMsg, ConfigSetMsg, ControlMessage, HelloMsg, SCHEMA_VERSION};
use arl_transport::quinn::{
    Acl, Capabilities, ClientEvent, SessionClient, SessionLimits, SessionServer,
};
use common::{roots, TestCa, TestLeaf};

const WINDOW: Duration = Duration::from_millis(2500);

/// Aplicador de efecto explícito de la prueba: acepta el valor pedido. Es un
/// fixture de software; no hay hardware ni límite inventado.
struct FixtureApply;

impl arl_proto::control::EffectivePolicy for FixtureApply {
    fn effective(
        &self,
        _key: &ConfigKey,
        requested: &ConfigValue,
    ) -> Result<ConfigValue, ErrorCode> {
        Ok(requested.clone())
    }
}

struct Script {
    messages: Vec<ControlMessage>,
    inbox: Arc<Mutex<Vec<ControlMessage>>>,
}

fn hello(id: &str) -> ControlMessage {
    ControlMessage::Hello(HelloMsg {
        request_id: id.into(),
        role: PeerRole::Android,
        version: "0.1.0".into(),
        protocol_version: SCHEMA_VERSION.into(),
    })
}

fn set(id: &str, expected: u64, value: bool) -> ControlMessage {
    ControlMessage::ConfigSet(ConfigSetMsg {
        request_id: id.into(),
        expected_revision: expected,
        key: ConfigKey::new("audio.mute").expect("clave del catálogo"),
        value: ConfigValue::Bool(value),
    })
}

fn get(id: &str) -> ControlMessage {
    ControlMessage::ConfigGet(ConfigGetMsg {
        request_id: id.into(),
        key: Some(ConfigKey::new("audio.mute").expect("clave del catálogo")),
    })
}

/// Cliente en su hilo: espera autorización, manda su guion y guarda lo que
/// vuelve. El socket es real; nadie le entrega datagramas en memoria.
fn run_client(mut client: SessionClient, socket: UdpSocket, script: Script, until: Instant) {
    socket
        .set_read_timeout(Some(Duration::from_millis(2)))
        .expect("timeout");
    let mut buf = [0u8; 2048];
    let mut authorized = false;
    let mut sent = false;
    while Instant::now() < until {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            let _ = client.handle_datagram(Instant::now(), from, &buf[..len]);
        }
        let now = Instant::now();
        let _ = client.handle_timeout(now);
        while let Some((to, bytes)) = client.poll_transmit(now) {
            let _ = socket.send_to(&bytes, to);
        }
        while let Some(event) = client.next_event() {
            if let ClientEvent::Authorized { .. } = event {
                authorized = true;
            }
        }
        if authorized && !sent {
            for message in &script.messages {
                let _ = client.send_control(message);
            }
            sent = true;
        }
        while let Some(message) = client.recv_control() {
            script.inbox.lock().expect("buzón").push(message);
        }
    }
}

fn client_for(
    ca: &TestCa,
    server_leaf: &TestLeaf,
    leaf: &TestLeaf,
    server_addr: SocketAddr,
) -> (SessionClient, UdpSocket) {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind cliente");
    let client = SessionClient::connect(
        leaf.certified(ca),
        roots(ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        server_addr,
        "localhost",
    )
    .expect("cliente");
    (client, socket)
}

fn grant(id: &str, write: bool) -> IdentityGrant {
    let mut actions = vec![RemoteAction::ConfigRead];
    if write {
        actions.push(RemoteAction::ConfigWrite);
    }
    IdentityGrant {
        device_id: id.into(),
        role: PeerRole::Android,
        actions,
    }
}

fn find_ack<'a>(inbox: &'a [ControlMessage], request: &str) -> &'a arl_proto::wire::AckMsg {
    inbox
        .iter()
        .find_map(|message| match message {
            ControlMessage::Ack(ack) if ack.request_id == request => Some(ack),
            _ => None,
        })
        .unwrap_or_else(|| panic!("sin ack para {request}: {inbox:?}"))
}

fn find_error<'a>(inbox: &'a [ControlMessage], request: &str) -> &'a arl_proto::control::ErrorMsg {
    inbox
        .iter()
        .find_map(|message| match message {
            ControlMessage::Error(error) if error.request_id == request => Some(error),
            _ => None,
        })
        .unwrap_or_else(|| panic!("sin error para {request}: {inbox:?}"))
}

#[test]
fn local_and_remote_share_one_revision_over_udp() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arlf4server", true, false);
    let writer = ca.leaf("arlf4writer", false, false);
    let reader = ca.leaf("arlf4reader", false, false);

    let server_socket = UdpSocket::bind("127.0.0.1:0").expect("bind servidor");
    let server_addr = server_socket.local_addr().expect("addr");

    let mut acl = Acl::new();
    // Control sí; micrófono y audio de retorno no. `config.write` no los abre.
    let caps = Capabilities::none()
        .with_control_read()
        .with_control_write();
    acl.allow(writer.pin, caps);
    acl.allow(reader.pin, caps);

    let mut server = SessionServer::new(
        server_leaf.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");

    let mut registry = ControlRegistry::new();
    registry
        .register(writer.pin, grant("writer_device", true))
        .expect("writer");
    registry
        .register(reader.pin, grant("reader_device", false))
        .expect("registro reader");
    let mut control = SharedControl::with_policy(registry, Box::new(FixtureApply));
    control
        .open_local(IdentityGrant {
            device_id: "local_admin".into(),
            role: PeerRole::LinuxCli,
            actions: vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
        })
        .expect("admin local");

    let writer_inbox = Arc::new(Mutex::new(Vec::new()));
    let reader_inbox = Arc::new(Mutex::new(Vec::new()));
    let (writer_client, writer_socket) = client_for(&ca, &server_leaf, &writer, server_addr);
    let (reader_client, reader_socket) = client_for(&ca, &server_leaf, &reader, server_addr);

    let until = Instant::now() + WINDOW;
    let writer_script = Script {
        messages: vec![hello("w-hello"), set("w-set", 0, true)],
        inbox: Arc::clone(&writer_inbox),
    };
    let reader_script = Script {
        messages: vec![hello("r-hello"), set("r-set", 0, false), get("r-get")],
        inbox: Arc::clone(&reader_inbox),
    };
    let writer_thread =
        thread::spawn(move || run_client(writer_client, writer_socket, writer_script, until));
    let reader_thread =
        thread::spawn(move || run_client(reader_client, reader_socket, reader_script, until));

    // El admin local escribe antes de que corra el servidor: la revisión
    // pasa a 1 y el `expected_revision: 0` remoto tiene que chocar.
    let local = control
        .local_call(&set("local-set", 0, true))
        .expect("local");
    match local.response {
        Some(ControlMessage::Ack(ack)) => {
            assert!(ack.applied, "el admin local aplica el primer cambio");
            assert_eq!(ack.revision, 1);
        }
        other => panic!("ack local esperado, llegó {other:?}"),
    }
    assert_eq!(control.revision(), 1);

    let (_producer, _consumer) = ring(480);
    let mut producer = _producer;
    let stats = serve_with_control(
        &server_socket,
        &mut server,
        &mut producer,
        until,
        Some(&mut control),
    )
    .expect("serve");
    writer_thread.join().expect("hilo writer");
    reader_thread.join().expect("hilo reader");

    assert!(
        stats.sessions_authorized >= 2,
        "ambos clientes se autorizan"
    );

    let writer_got = writer_inbox.lock().expect("buzón").clone();
    let reader_got = reader_inbox.lock().expect("buzón").clone();

    let writer_caps = writer_got.iter().find_map(|message| match message {
        ControlMessage::Capabilities(caps) if caps.request_id == "w-hello" => Some(caps),
        _ => None,
    });
    let writer_caps = writer_caps.expect("capabilities del writer");
    assert_eq!(
        writer_caps.revision, 1,
        "la revisión anunciada ya es la local"
    );
    assert_eq!(
        writer_caps.capabilities.remote_actions,
        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite]
    );

    let writer_set = find_error(&writer_got, "w-set");
    assert_eq!(writer_set.code, ErrorCode::Conflict);
    assert_eq!(writer_set.current_revision, Some(1));

    let reader_set = find_error(&reader_got, "r-set");
    assert_eq!(
        reader_set.code,
        ErrorCode::Denied,
        "config.read no autoriza config.set"
    );

    let reader_get = find_ack(&reader_got, "r-get");
    assert_eq!(
        reader_get.revision, 1,
        "el lector ve la revisión del admin local"
    );

    let local_get = control
        .local_call(&get("local-get"))
        .expect("lectura local");
    match local_get.response {
        Some(ControlMessage::Ack(ack)) => assert_eq!(ack.revision, 1),
        other => panic!("lectura local esperada, llegó {other:?}"),
    }
    assert_eq!(control.revision(), 1, "nadie avanzó la revisión compartida");
}

/// Cliente que manda su guion y deja de leer. Así se llena la cola de
/// respuestas del servidor sin un seam: el peer real no drena.
fn run_stalled(
    mut client: SessionClient,
    socket: UdpSocket,
    messages: Vec<ControlMessage>,
    until: Instant,
) {
    socket
        .set_read_timeout(Some(Duration::from_millis(2)))
        .expect("timeout");
    let mut buf = [0u8; 2048];
    let mut authorized = false;
    let mut sent = false;
    while Instant::now() < until {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            let _ = client.handle_datagram(Instant::now(), from, &buf[..len]);
        }
        let now = Instant::now();
        let _ = client.handle_timeout(now);
        while let Some((to, bytes)) = client.poll_transmit(now) {
            let _ = socket.send_to(&bytes, to);
        }
        while let Some(event) = client.next_event() {
            if let ClientEvent::Authorized { .. } = event {
                authorized = true;
            }
        }
        if authorized && !sent {
            for message in &messages {
                let _ = client.send_control(message);
            }
            sent = true;
        }
        // Sigue bombeando el transporte para que los frames salgan, pero no
        // llama a `recv_control`: el peer no drena sus respuestas.
    }
}

/// Cliente sano: `hello`, lee la revisión que informa `capabilities` y recién
/// entonces manda su `config.set` con esa revisión. No asume un número fijo
/// porque otros peers pueden haber movido la revisión antes.
fn run_healthy(
    mut client: SessionClient,
    socket: UdpSocket,
    inbox: Arc<Mutex<Vec<ControlMessage>>>,
    until: Instant,
) {
    socket
        .set_read_timeout(Some(Duration::from_millis(2)))
        .expect("timeout");
    let mut buf = [0u8; 2048];
    let mut authorized = false;
    let mut sent_hello = false;
    let mut sent_set = false;
    while Instant::now() < until {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            let _ = client.handle_datagram(Instant::now(), from, &buf[..len]);
        }
        let now = Instant::now();
        let _ = client.handle_timeout(now);
        while let Some((to, bytes)) = client.poll_transmit(now) {
            let _ = socket.send_to(&bytes, to);
        }
        while let Some(event) = client.next_event() {
            if let ClientEvent::Authorized { .. } = event {
                authorized = true;
            }
        }
        if authorized && !sent_hello {
            let _ = client.send_control(&hello("h-hello"));
            sent_hello = true;
        }
        while let Some(message) = client.recv_control() {
            if let ControlMessage::Capabilities(caps) = &message {
                if !sent_set {
                    let _ = client.send_control(&gain_set("h-set", caps.revision, 3.0));
                    sent_set = true;
                }
            }
            inbox.lock().expect("buzón").push(message);
        }
    }
}

fn gain_set(id: &str, expected: u64, db: f64) -> ControlMessage {
    ControlMessage::ConfigSet(ConfigSetMsg {
        request_id: id.into(),
        expected_revision: expected,
        key: ConfigKey::new("audio.gain_db").expect("clave del catálogo"),
        value: ConfigValue::Number(db),
    })
}

/// Hello con `protocol_version` desconocido, como bytes de framing reales.
/// El encoder de `SessionClient::send_control` lo rechaza antes de escribir,
/// así que el frame se arma a mano con `wire::encode_frame`.
fn raw_bad_hello(id: &str) -> Vec<u8> {
    let payload = format!(
        "{{\"schema_version\":\"arl.control/v1\",\"type\":\"hello\",\"request_id\":\"{id}\",\"payload\":{{\"role\":\"android\",\"version\":\"0.1.0\",\"protocol_version\":\"arl.control/v0\"}}}}"
    );
    wire::encode_frame(payload.as_bytes()).expect("framing de prueba")
}

/// Manda el frame malo y, en el mismo stream y sin esperar respuesta, el set.
/// Si el cierre funciona, el servidor no procesa el set.
fn run_bad_then_set(
    mut client: SessionClient,
    socket: UdpSocket,
    bad: Vec<u8>,
    later: ControlMessage,
    until: Instant,
    inbox: Arc<Mutex<Vec<ControlMessage>>>,
    closed: Arc<AtomicBool>,
) {
    socket
        .set_read_timeout(Some(Duration::from_millis(2)))
        .expect("timeout");
    let mut buf = [0u8; 2048];
    let mut sent = false;
    while Instant::now() < until {
        if let Ok((len, from)) = socket.recv_from(&mut buf) {
            let _ = client.handle_datagram(Instant::now(), from, &buf[..len]);
        }
        let now = Instant::now();
        let _ = client.handle_timeout(now);
        while let Some((to, bytes)) = client.poll_transmit(now) {
            let _ = socket.send_to(&bytes, to);
        }
        while let Some(event) = client.next_event() {
            match event {
                ClientEvent::Authorized { .. } if !sent => {
                    client.send_control_raw(bad.clone()).expect("frame malo");
                    let _ = client.send_control(&later);
                    sent = true;
                }
                ClientEvent::Closed => closed.store(true, Ordering::SeqCst),
                _ => {}
            }
        }
        while let Some(message) = client.recv_control() {
            inbox.lock().expect("buzón").push(message);
        }
    }
}

/// Una versión no soportada cierra esa sesión y no mueve la revisión. El
/// admin local sigue leyendo la misma.
#[test]
fn unsupported_version_closes_session_without_mutating() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arlf4server", true, false);
    let peer = ca.leaf("arlf4badver", false, false);

    let server_socket = UdpSocket::bind("127.0.0.1:0").expect("bind servidor");
    let server_addr = server_socket.local_addr().expect("addr");
    let mut acl = Acl::new();
    acl.allow(
        peer.pin,
        Capabilities::none()
            .with_control_read()
            .with_control_write(),
    );
    let mut server = SessionServer::new(
        server_leaf.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");

    let mut registry = ControlRegistry::new();
    registry
        .register(peer.pin, grant("badver_device", true))
        .expect("registro");
    let mut control = SharedControl::with_policy(registry, Box::new(FixtureApply));
    control
        .open_local(IdentityGrant {
            device_id: "local_admin".into(),
            role: PeerRole::LinuxCli,
            actions: vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
        })
        .expect("admin local");

    let (client, socket) = client_for(&ca, &server_leaf, &peer, server_addr);
    let inbox = Arc::new(Mutex::new(Vec::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let until = Instant::now() + WINDOW;
    let thread = thread::spawn({
        let inbox = Arc::clone(&inbox);
        let closed = Arc::clone(&closed);
        move || {
            run_bad_then_set(
                client,
                socket,
                raw_bad_hello("bad-hello"),
                set("bad-set", 0, true),
                until,
                inbox,
                closed,
            )
        }
    });

    let (_producer, _consumer) = ring(480);
    let mut producer = _producer;
    let stats = serve_with_control(
        &server_socket,
        &mut server,
        &mut producer,
        until,
        Some(&mut control),
    )
    .expect("serve no aborta por el cierre de una sesión");
    thread.join().expect("hilo");

    assert!(
        stats.sessions_authorized >= 1,
        "la sesión llega a autorizarse"
    );
    assert!(
        closed.load(Ordering::SeqCst),
        "el peer observa el cierre tras version_unsupported"
    );
    let got = inbox.lock().expect("buzón").clone();
    let refused = find_error(&got, "bad-hello");
    assert_eq!(
        refused.code,
        ErrorCode::VersionUnsupported,
        "el error version_unsupported llega antes del cierre"
    );
    assert!(
        got.iter().all(|message| match message {
            ControlMessage::Error(error) => error.request_id != "bad-set",
            ControlMessage::Ack(ack) => ack.request_id != "bad-set",
            _ => true,
        }),
        "el set posterior al cierre no recibe respuesta: {got:?}"
    );
    assert_eq!(control.revision(), 0, "el rechazo no mueve la revisión");
    let local = control.local_call(&get("local-after")).expect("local");
    match local.response {
        Some(ControlMessage::Ack(ack)) => assert_eq!(ack.revision, 0),
        other => panic!("el admin local sigue vivo, llegó {other:?}"),
    }
}

/// Un peer que no lee llena su cola. El daemon aísla esa sesión y el otro
/// cliente, más el admin local, siguen aplicando sobre la misma revisión.
#[test]
fn stalled_peer_is_isolated_and_others_keep_serving() {
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arlf4server", true, false);
    let stalled = ca.leaf("arlf4stalled", false, false);
    let healthy = ca.leaf("arlf4healthy", false, false);

    let server_socket = UdpSocket::bind("127.0.0.1:0").expect("bind servidor");
    let server_addr = server_socket.local_addr().expect("addr");
    let mut acl = Acl::new();
    let caps = Capabilities::none()
        .with_control_read()
        .with_control_write();
    acl.allow(stalled.pin, caps);
    acl.allow(healthy.pin, caps);
    let mut server = SessionServer::new(
        server_leaf.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");

    let mut registry = ControlRegistry::new();
    registry
        .register(stalled.pin, grant("stalled_device", true))
        .expect("registro stalled");
    registry
        .register(healthy.pin, grant("healthy_device", true))
        .expect("registro healthy");
    let mut control = SharedControl::with_policy(registry, Box::new(FixtureApply));
    control
        .open_local(IdentityGrant {
            device_id: "local_admin".into(),
            role: PeerRole::LinuxCli,
            actions: vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
        })
        .expect("admin local");

    // Cinco sets aplicados son diez frames (ACK + evento). La cola admite
    // ocho, así que el peer que no lee desborda la suya.
    let stalled_messages = vec![
        hello("s-hello"),
        gain_set("s-0", 0, 0.5),
        gain_set("s-1", 1, 1.0),
        gain_set("s-2", 2, 1.5),
        gain_set("s-3", 3, 2.0),
        gain_set("s-4", 4, 2.5),
    ];
    let (stalled_client, stalled_socket) = client_for(&ca, &server_leaf, &stalled, server_addr);
    let healthy_inbox = Arc::new(Mutex::new(Vec::new()));
    let (healthy_client, healthy_socket) = client_for(&ca, &server_leaf, &healthy, server_addr);

    let until = Instant::now() + WINDOW;
    let stalled_thread =
        thread::spawn(move || run_stalled(stalled_client, stalled_socket, stalled_messages, until));
    let healthy_thread = thread::spawn({
        let inbox = Arc::clone(&healthy_inbox);
        move || run_healthy(healthy_client, healthy_socket, inbox, until)
    });

    let (_producer, _consumer) = ring(480);
    let mut producer = _producer;
    let stats = serve_with_control(
        &server_socket,
        &mut server,
        &mut producer,
        until,
        Some(&mut control),
    )
    .expect("QueueFull de un peer no tumba el daemon");
    stalled_thread.join().expect("hilo stalled");
    healthy_thread.join().expect("hilo healthy");

    assert!(stats.sessions_authorized >= 2, "entran los dos clientes");
    let healthy_got = healthy_inbox.lock().expect("buzón").clone();
    // El peer sano tiene que recibir una respuesta definida para su set: ACK
    // o conflicto. Un conflicto es válido aquí porque el peer aislado mueve la
    // revisión en paralelo; lo que se exige es que el peer sano siga vivo.
    let answered = healthy_got.iter().any(|message| match message {
        ControlMessage::Ack(ack) => ack.request_id == "h-set",
        ControlMessage::Error(error) => error.request_id == "h-set",
        _ => false,
    });
    assert!(
        answered,
        "el peer sano recibe respuesta a su set: {healthy_got:?}"
    );
    let local = control.local_call(&get("local-after")).expect("local");
    match local.response {
        Some(ControlMessage::Ack(ack)) => {
            assert_eq!(ack.revision, control.revision());
            assert!(ack.revision >= 1, "la revisión avanzó pese al peer aislado");
        }
        other => panic!("el admin local sigue vivo, llegó {other:?}"),
    }
}
