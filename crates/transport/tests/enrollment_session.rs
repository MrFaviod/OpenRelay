//! E2E de aprobación: el resultado del enrolamiento trae una hoja de sesión
//! firmada por la CA local y con ella el cliente abre una sesión mTLS real y
//! entrega audio.

mod common;

use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use openrelay_audio_core::{AudioFrame, Flags, SAMPLE_COUNT};
use openrelay_transport::enrollment::{EnrollClient, EnrollOutcome, EnrollServer, EnrollmentEvent};
use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{
    Acl, Capabilities, ServerEvent, SessionClient, SessionLimits, SessionServer,
};
use rustls::pki_types::CertificateDer;

use common::{exchange, roots, token, TestCa, CLIENT_ADDR, SERVER_ADDR};

#[test]
fn enrollment_approval_issues_a_leaf_that_opens_a_session() {
    let ca = Arc::new(TestCa::new());
    let daemon = ca.leaf("openrelay-daemon-e2e", true, false);
    let android = ca.leaf("openrelay-android-e2e", false, false);
    let android_spki = android.spki_sha256();

    let window = token();
    let mut server = EnrollServer::new(daemon.certified(&ca)).expect("servidor enroll");
    server
        .open_window(window.clone(), Duration::from_secs(30), Instant::now())
        .expect("ventana");

    // Servidor real sobre UDP: recibe el token, espera la decisión local,
    // firma la hoja del dispositivo y escribe el resultado.
    let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("socket enroll");
    socket
        .set_read_timeout(Some(Duration::from_millis(20)))
        .expect("timeout");
    let server_addr = socket.local_addr().expect("addr enroll");
    let ca_for_thread = Arc::clone(&ca);
    let driver = thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut approved_at: Option<Instant> = None;
        while Instant::now() < deadline {
            let now = Instant::now();
            match socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let _ = server.handle_datagram(now, from, &buf[..len]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("lectura UDP: {error}"),
            }
            server.handle_timeout(now).expect("timeout del servidor");
            while let Some((to, bytes)) = server.poll_transmit(now) {
                let _ = socket.send_to(&bytes, to);
            }
            while let Some(event) = server.next_event() {
                if let EnrollmentEvent::Accepted(_) = event {
                    let spki = server.pending_spki().expect("SPKI pendiente").to_vec();
                    let leaf = ca_for_thread.issue_client_leaf(&spki);
                    let outcome = EnrollOutcome {
                        client_leaf_der: leaf,
                        session_ca_der: ca_for_thread.cert_der().as_ref().to_vec(),
                        session_port: 47000,
                    };
                    server.approve(now, &outcome).expect("aprobación");
                    while let Some((to, bytes)) = server.poll_transmit(now) {
                        let _ = socket.send_to(&bytes, to);
                    }
                    approved_at = Some(Instant::now());
                }
            }
            if approved_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(200)) {
                break;
            }
        }
    });

    let mut client = EnrollClient::connect(
        roots(&ca),
        daemon.pin,
        Instant::now(),
        server_addr,
        "localhost",
    )
    .expect("cliente enroll");
    let outcome = client
        .send_token(
            &window,
            identity::extract_spki_der(android.cert.der()).expect("SPKI del dispositivo"),
        )
        .expect("resultado de la aprobación");
    driver.join().expect("driver de enrolamiento");

    assert_eq!(outcome.session_port, 47000);
    assert_eq!(
        outcome.session_ca_der,
        ca.cert_der().as_ref(),
        "la CA de sesión es la CA local"
    );
    let leaf_pin =
        SpkiPin::from_certificate_der(&CertificateDer::from(outcome.client_leaf_der.clone()))
            .expect("SPKI de la hoja emitida");
    assert_eq!(
        leaf_pin, android_spki,
        "la hoja corresponde a la clave del dispositivo"
    );

    // La hoja emitida abre una sesión mTLS contra el servidor persistente.
    let client_identity = identity::certified_key(
        vec![CertificateDer::from(outcome.client_leaf_der), ca.cert_der()],
        android.signer.clone(),
    )
    .expect("identidad de sesión");
    let mut acl = Acl::new();
    acl.allow(android_spki, Capabilities::none().with_mic());
    let mut session_server = SessionServer::new(
        daemon.certified(&ca),
        roots(&ca),
        acl,
        SessionLimits::default(),
    )
    .expect("servidor de sesión");
    let mut session_client = SessionClient::connect(
        client_identity.into(),
        roots(&ca),
        daemon.pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .expect("cliente de sesión");

    exchange(
        &mut session_client,
        CLIENT_ADDR,
        &mut session_server,
        SERVER_ADDR,
        24,
    )
    .expect("handshake mTLS");
    let session = std::iter::from_fn(|| session_server.next_event())
        .find_map(|event| match event {
            ServerEvent::Authorized { session, .. } => Some(session),
            _ => None,
        })
        .expect("sesión autorizada con la hoja emitida");

    let mut samples = [0i16; SAMPLE_COUNT];
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample = index as i16 - 100;
    }
    let audio = AudioFrame::new(7, 1, Flags::NONE, samples).expect("frame");
    session_client
        .send_audio(&audio)
        .expect("audio del cliente");
    exchange(
        &mut session_client,
        CLIENT_ADDR,
        &mut session_server,
        SERVER_ADDR,
        8,
    )
    .expect("audio en vuelo");
    assert_eq!(
        session_server.recv_audio(session),
        Some(audio),
        "el audio del dispositivo llega al servidor"
    );
}
