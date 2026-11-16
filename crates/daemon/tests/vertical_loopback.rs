//! Vertical loopback F2-14a: cliente QUIC/mTLS real sobre UDP 127.0.0.1 ->
//! `SessionServer` -> `serve` -> `PipeWireSource::producer()`.
//!
//! Requiere un daemon PipeWire privado en ejecución (PIPEWIRE_REMOTE,
//! XDG_RUNTIME_DIR), por eso es `#[ignore]`:
//! `cargo test -p arl-daemon --test vertical_loopback -- --ignored`.

#[path = "common/mod.rs"]
mod common;

use std::net::{SocketAddr, UdpSocket};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

use arl_audio_core::{AudioFrame, Flags, Samples, SAMPLE_COUNT};
use arl_audio_linux::{PipeWireSource, SampleProducer};
use arl_daemon::{fnv1a_extend, serve, ServeStats, FNV_OFFSET};
use arl_transport::quinn::{
    Acl, Capabilities, ClientEvent, SessionClient, SessionLimits, SessionServer,
};
use common::{roots, TestCa, TestLeaf};

const FRAMES: u32 = 100;
const PACE: Duration = Duration::from_millis(5);
const WINDOW: Duration = Duration::from_millis(2500);
const RING_SAMPLES: usize = SAMPLE_COUNT * 200;

static INIT: Once = Once::new();

/// Patrón determinista: cada muestra depende de (seq, índice).
fn pattern(seq: u32) -> Samples {
    let mut samples = [0i16; SAMPLE_COUNT];
    for (i, sample) in samples.iter_mut().enumerate() {
        let v = (i64::from(seq) * 37 + i as i64 * 11) % 2000 - 1000;
        *sample = v as i16;
    }
    samples
}

fn expected_checksum() -> u64 {
    (0..FRAMES).fold(FNV_OFFSET, |hash, seq| {
        fnv1a_extend(hash, &pattern(seq)[..])
    })
}

/// Cliente en su hilo: tras `Authorized` envía `FRAMES` frames a ritmo fijo
/// hasta `until`. Devuelve (frames aceptados por `send_audio`, autorizado).
fn run_client(mut client: SessionClient, socket: UdpSocket, until: Instant) -> (u32, bool) {
    socket
        .set_read_timeout(Some(Duration::from_millis(1)))
        .expect("timeout de lectura");
    let mut buf = [0u8; 2048];
    let mut next_send = Instant::now();
    let mut next_seq = 0u32;
    let mut sent = 0u32;
    let mut authorized = false;

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
                next_send = Instant::now();
            }
        }
        if authorized && next_seq < FRAMES && now >= next_send {
            let frame = AudioFrame::new(1, next_seq, Flags::NONE, pattern(next_seq))
                .expect("frame de test");
            if client.send_audio(&frame).is_ok() {
                sent += 1;
            }
            next_seq += 1;
            // Programación absoluta: no acumula deriva frente al reloj de
            // reproducción del jitter (5 ms por frame).
            next_send += PACE;
        }
    }
    (sent, authorized)
}

/// Un escenario: servidor con ACL (cliente `client_leaf` sólo si `allowed`),
/// cliente QUIC real en otro hilo, `serve` en este hilo hasta `WINDOW`.
fn scenario(
    ca: &TestCa,
    server_leaf: &TestLeaf,
    client_leaf: &TestLeaf,
    allowed: bool,
    producer: &mut SampleProducer,
) -> (ServeStats, u32, bool) {
    let server_socket = UdpSocket::bind("127.0.0.1:0").expect("bind servidor");
    let server_addr: SocketAddr = server_socket.local_addr().expect("addr servidor");
    let client_socket = UdpSocket::bind("127.0.0.1:0").expect("bind cliente");

    let mut acl = Acl::new();
    if allowed {
        acl.allow(client_leaf.pin, Capabilities::none().with_mic());
    }
    let mut server = SessionServer::new(
        server_leaf.certified(ca),
        roots(ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor");
    let client = SessionClient::connect(
        client_leaf.certified(ca),
        roots(ca),
        server_leaf.pin,
        SessionLimits::default(),
        Instant::now(),
        server_addr,
        "localhost",
    )
    .expect("cliente");

    let until = Instant::now() + WINDOW;
    let handle = thread::spawn(move || run_client(client, client_socket, until));
    let stats = serve(&server_socket, &mut server, producer, until).expect("serve");
    let (sent, authorized) = handle.join().expect("hilo cliente");
    (stats, sent, authorized)
}

#[test]
#[ignore = "requiere PipeWire privado: PIPEWIRE_REMOTE y XDG_RUNTIME_DIR"]
fn vertical_loopback() {
    INIT.call_once(arl_audio_linux::init);
    let mut source = PipeWireSource::open(RING_SAMPLES).expect("PipeWire privado");
    let ca = TestCa::new();
    let server_leaf = ca.leaf("arl-daemon-server", true, false);
    let allowed_leaf = ca.leaf("arl-daemon-client-ok", false, false);
    let rogue_leaf = ca.leaf("arl-daemon-client-rogue", false, false);

    // 1) Cliente autorizado por ACL: todos los frames llegan al producer.
    let (stats, sent, authorized) =
        scenario(&ca, &server_leaf, &allowed_leaf, true, source.producer());
    assert!(
        authorized,
        "el cliente autorizado debe completar el handshake"
    );
    assert_eq!(sent, FRAMES, "el cliente envía todos los frames");
    assert_eq!(stats.sessions_authorized, 1, "una sesión autorizada");
    assert_eq!(stats.frames_received, u64::from(FRAMES), "frames recibidos");
    assert_eq!(stats.frames_written, u64::from(FRAMES), "frames escritos");
    assert_eq!(
        stats.samples_written,
        u64::from(FRAMES) * SAMPLE_COUNT as u64,
        "muestras escritas"
    );
    assert_eq!(stats.samples_dropped, 0, "sin desbordamiento del anillo");
    assert_eq!(stats.checksum, expected_checksum(), "muestras en orden");

    // 2) Cliente sin entrada en la ACL: ningún audio entra en el daemon.
    let (rogue, _rogue_sent, _rogue_auth) =
        scenario(&ca, &server_leaf, &rogue_leaf, false, source.producer());
    assert_eq!(rogue.sessions_authorized, 0, "sin sesión autorizada");
    assert_eq!(rogue.frames_received, 0, "ningún frame del cliente sin ACL");
    assert_eq!(rogue.frames_written, 0, "ningún frame escrito");
    assert_eq!(rogue.samples_written, 0, "ninguna muestra escrita");
    assert_eq!(rogue.checksum, FNV_OFFSET, "huella intacta");
}
