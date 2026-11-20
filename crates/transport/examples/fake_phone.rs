//! Synthetic phone for the end-to-end harness.
//!
//! It speaks the real product protocol: parses the pairing URI that
//! `openrelay pair` prints, generates a P-256 identity, enrolls with the real
//! [`EnrollClient`] (pin-only trust, exactly like the app after scanning the
//! QR), opens a real mTLS [`SessionClient`] and streams a 440 Hz sine as
//! PCM16LE mono 48 kHz in 240-sample frames, paced in real time.
//!
//! Two modes:
//!
//! ```text
//! fake_phone --uri 'openrelay://pair?...' [--seconds 8] [--save-identity PATH]
//! fake_phone --identity PATH [--expect-refused]
//! ```
//!
//! `--expect-refused` succeeds when the server closes or rejects the session
//! instead of carrying audio: it is the negative test for `devices.remove`.
//!
//! Exit codes: 0 success, 1 protocol/transport failure, 2 usage.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openrelay_audio_core::{AudioFrame, Flags, SAMPLE_COUNT, SAMPLE_RATE_HZ};
use openrelay_transport::enrollment::{EnrollClient, EnrollmentToken};
use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{ClientEvent, SessionClient, SessionLimits};
use rcgen::{CertificateParams, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::PrivateKeyDer;
use rustls::RootCertStore;
use serde_json::{json, Value};

const TONE_HZ: f64 = 440.0;
const AMPLITUDE: f64 = 0.25;
const SERVER_NAME: &str = "openrelay";
const FRAME_PERIOD: Duration = Duration::from_micros(5_000);

fn main() {
    match run() {
        Ok(()) => {}
        Err(error) => {
            eprintln!("fake_phone: {error}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut uri: Option<String> = None;
    let mut identity_path: Option<String> = None;
    let mut save_identity: Option<String> = None;
    let mut seconds = 8.0f64;
    let mut expect_refused = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--uri" => uri = Some(args.next().ok_or("--uri requiere el URI")?),
            "--identity" => identity_path = Some(args.next().ok_or("--identity requiere ruta")?),
            "--save-identity" => {
                save_identity = Some(args.next().ok_or("--save-identity requiere ruta")?)
            }
            "--seconds" => {
                seconds = args
                    .next()
                    .ok_or("--seconds requiere un número")?
                    .parse()
                    .map_err(|_| "--seconds inválido")?
            }
            "--expect-refused" => expect_refused = true,
            other => return Err(format!("argumento desconocido: {other}")),
        }
    }

    if let Some(path) = identity_path {
        let saved = load_identity(&path)?;
        return connect_and_stream(saved, expect_refused, seconds);
    }

    let uri = uri.ok_or("falta --uri o --identity")?;
    let pair = PairUri::parse(&uri)?;
    let now = Instant::now();

    // Identidad del teléfono: clave ECDSA P-256 y certificado autofirmado. El
    // daemon sólo recibe el SPKI y emite la hoja; la clave no sale de aquí.
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|error| format!("clave P-256: {error}"))?;
    let mut params = CertificateParams::new(vec!["openrelay-fake-phone".to_owned()])
        .map_err(|e| e.to_string())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "openrelay-fake-phone");
    let cert = params
        .self_signed(&key)
        .map_err(|error| format!("certificado: {error}"))?;
    let spki_der = identity::extract_spki_der(cert.der().as_ref())
        .map_err(|error| format!("SPKI: {error}"))?
        .to_vec();

    let roots = Arc::new(RootCertStore::empty());
    let mut client =
        EnrollClient::connect(roots, pair.server_pin, now, pair.enroll_addr, SERVER_NAME)
            .map_err(|error| format!("enrolamiento: {error}"))?;
    eprintln!(
        "fake_phone: enrolándose contra {} (token de la ventana del QR)",
        pair.enroll_addr
    );
    let outcome = client
        .send_token(&pair.token, &spki_der)
        .map_err(|error| format!("token: {error}"))?;
    eprintln!(
        "fake_phone: hoja emitida ({} bytes), CA de sesión {} bytes, puerto {}",
        outcome.client_leaf_der.len(),
        outcome.session_ca_der.len(),
        outcome.session_port
    );

    let signer = private_key_signer(&key)?;
    let session_identity = identity::certified_key(
        vec![
            rustls::pki_types::CertificateDer::from(outcome.client_leaf_der.clone()),
            rustls::pki_types::CertificateDer::from(outcome.session_ca_der.clone()),
        ],
        signer,
    )
    .map_err(|error| format!("identidad de sesión: {error}"))?;

    let saved = SavedIdentity {
        pkcs8_der: key.serialize_der(),
        leaf_der: outcome.client_leaf_der,
        ca_der: outcome.session_ca_der,
        server_pin: pair.server_pin,
        session_addr: SocketAddr::new(
            pair.enroll_addr.ip(),
            pair.session_port_or(outcome.session_port),
        ),
    };
    if let Some(path) = save_identity {
        save(&path, &saved)?;
        eprintln!("fake_phone: identidad guardada en {path}");
    }

    let mut session = SessionClient::connect(
        Arc::new(session_identity),
        Arc::new(RootCertStore::empty()),
        saved.server_pin,
        SessionLimits::default(),
        Instant::now(),
        saved.session_addr,
        SERVER_NAME,
    )
    .map_err(|error| format!("sesión: {error}"))?;
    eprintln!("fake_phone: sesión QUIC hacia {}", saved.session_addr);
    stream(&mut session, seconds, expect_refused)
}

fn private_key_signer(key: &KeyPair) -> Result<Arc<dyn rustls::sign::SigningKey>, String> {
    let der = PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        key.serialize_der(),
    ));
    rustls::crypto::ring::sign::any_supported_type(&der).map_err(|error| format!("signer: {error}"))
}

struct SavedIdentity {
    pkcs8_der: Vec<u8>,
    leaf_der: Vec<u8>,
    ca_der: Vec<u8>,
    server_pin: SpkiPin,
    session_addr: SocketAddr,
}

fn save(path: &str, saved: &SavedIdentity) -> Result<(), String> {
    let doc = json!({
        "pkcs8_hex": to_hex(&saved.pkcs8_der),
        "leaf_hex": to_hex(&saved.leaf_der),
        "ca_hex": to_hex(&saved.ca_der),
        "server_pin": saved.server_pin.to_hex(),
        "session_addr": saved.session_addr.to_string(),
    });
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?,
    )
    .map_err(|error| format!("{path}: {error}"))
}

fn load_identity(path: &str) -> Result<SavedIdentity, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
    let doc: Value = serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))?;
    let field = |name: &str| -> Result<String, String> {
        doc[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{path}: falta {name}"))
    };
    Ok(SavedIdentity {
        pkcs8_der: from_hex(&field("pkcs8_hex")?)?,
        leaf_der: from_hex(&field("leaf_hex")?)?,
        ca_der: from_hex(&field("ca_hex")?)?,
        server_pin: SpkiPin::from_hex(&field("server_pin")?)
            .ok_or_else(|| format!("{path}: pin inválido"))?,
        session_addr: field("session_addr")?
            .parse()
            .map_err(|_| format!("{path}: session_addr inválido"))?,
    })
}

fn connect_and_stream(
    saved: SavedIdentity,
    expect_refused: bool,
    seconds: f64,
) -> Result<(), String> {
    let key = KeyPair::try_from(saved.pkcs8_der.as_slice())
        .map_err(|error| format!("clave guardada: {error}"))?;
    let signer = private_key_signer(&key)?;
    let session_identity = identity::certified_key(
        vec![
            rustls::pki_types::CertificateDer::from(saved.leaf_der.clone()),
            rustls::pki_types::CertificateDer::from(saved.ca_der.clone()),
        ],
        signer,
    )
    .map_err(|error| format!("identidad guardada: {error}"))?;
    let mut session = SessionClient::connect(
        Arc::new(session_identity),
        Arc::new(RootCertStore::empty()),
        saved.server_pin,
        SessionLimits::default(),
        Instant::now(),
        saved.session_addr,
        SERVER_NAME,
    )
    .map_err(|error| format!("sesión: {error}"))?;
    eprintln!("fake_phone: reconectando a {}", saved.session_addr);
    stream(&mut session, seconds, expect_refused)
}

/// Pumps the client and, once authorized, sends the tone in real time.
///
/// With `expect_refused`, success means the server never lets the session
/// carry audio: a `Rejected`/`Closed` event arrives, or authorization never
/// happens inside the deadline.
fn stream(session: &mut SessionClient, seconds: f64, expect_refused: bool) -> Result<(), String> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|error| format!("socket: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_millis(2)))
        .map_err(|error| format!("timeout UDP: {error}"))?;

    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(1.0));
    let auth_deadline = Instant::now() + Duration::from_secs(6);
    let mut authorized_at: Option<Instant> = None;
    let mut closed = false;
    let mut buf = [0u8; 2048];
    let mut phase = 0.0f64;
    let mut seq = 0u32;
    let mut sent = 0u64;
    let mut next_frame = Instant::now();

    loop {
        let now = Instant::now();
        while let Some((to, bytes)) = session.poll_transmit(now) {
            let _ = socket.send_to(&bytes, to);
        }
        match socket.recv_from(&mut buf) {
            Ok((len, from)) => {
                let _ = session.handle_datagram(Instant::now(), from, &buf[..len]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("lectura UDP: {error}")),
        }
        let _ = session.handle_timeout(Instant::now());

        while let Some(event) = session.next_event() {
            match event {
                ClientEvent::Authorized { server } => {
                    let pin: String = server.to_hex();
                    if expect_refused {
                        eprintln!(
                            "fake_phone: TLS establecido (servidor {}); esperando autorización de aplicación",
                            &pin[..12]
                        );
                    } else {
                        eprintln!("fake_phone: sesión autorizada, servidor {}", &pin[..12]);
                    }
                    authorized_at = Some(Instant::now());
                }
                ClientEvent::Rejected(reason) => {
                    eprintln!("fake_phone: rechazado por el servidor: {reason}");
                    closed = true;
                }
                ClientEvent::Closed => {
                    let reason = session.close_reason().unwrap_or("sin motivo").to_owned();
                    eprintln!("fake_phone: sesión cerrada por el servidor ({reason})");
                    closed = true;
                }
            }
        }

        if expect_refused {
            if closed {
                let reason = session.close_reason().unwrap_or("sin motivo");
                if reason == "unauthorized" {
                    println!(
                        "REFUSED: el servidor rechazó la sesión en la autorización (unauthorized)"
                    );
                    return Ok(());
                }
                return Err(format!(
                    "el servidor cerró la sesión pero no como rechazo de ACL ({reason})"
                ));
            }
            if Instant::now() >= auth_deadline {
                return Err("el servidor no rechazó la sesión en 6 s".to_owned());
            }
            continue;
        }

        if authorized_at.is_none() {
            if Instant::now() >= auth_deadline {
                return Err("el servidor no autorizó la sesión en 6 s".to_owned());
            }
            continue;
        }
        if closed {
            return Err("la sesión se cerró mientras se enviaba audio".to_owned());
        }
        if Instant::now() >= deadline {
            break;
        }
        if Instant::now() >= next_frame {
            let mut samples = [0i16; SAMPLE_COUNT];
            for sample in &mut samples {
                *sample = (phase.sin() * AMPLITUDE * f64::from(i16::MAX)) as i16;
                phase += std::f64::consts::TAU * TONE_HZ / f64::from(SAMPLE_RATE_HZ);
                if phase > std::f64::consts::TAU {
                    phase -= std::f64::consts::TAU;
                }
            }
            let frame = AudioFrame::new(1, seq, Flags::NONE, samples)
                .map_err(|error| format!("frame: {error}"))?;
            session
                .send_audio(&frame)
                .map_err(|error| format!("audio: {error}"))?;
            seq = seq.wrapping_add(1);
            sent += 1;
            next_frame += FRAME_PERIOD;
            if next_frame < Instant::now() {
                next_frame = Instant::now();
            }
        }
    }

    println!(
        "STREAMED frames={sent} tone_hz={TONE_HZ} rate_hz={SAMPLE_RATE_HZ} samples_per_frame={SAMPLE_COUNT}"
    );
    Ok(())
}

/// `openrelay://pair?v=1&h=HOST:PORT&pin=HEX&t=HEX`
struct PairUri {
    enroll_addr: SocketAddr,
    server_pin: SpkiPin,
    token: EnrollmentToken,
    session_port: Option<u16>,
}

impl PairUri {
    fn parse(uri: &str) -> Result<Self, String> {
        let rest = uri
            .strip_prefix("openrelay://pair?")
            .ok_or("el URI no es openrelay://pair?…")?;
        let mut host = None;
        let mut pin = None;
        let mut token = None;
        let mut version = None;
        for pair in rest.split('&') {
            let (key, value) = pair.split_once('=').ok_or("parámetro sin '='")?;
            match key {
                "v" => version = Some(value.to_owned()),
                "h" => host = Some(value.to_owned()),
                "pin" => pin = Some(value.to_owned()),
                "t" => token = Some(value.to_owned()),
                _ => {}
            }
        }
        if version.as_deref() != Some("1") {
            return Err(format!("versión de URI no soportada: {version:?}"));
        }
        let enroll_addr: SocketAddr = host
            .ok_or("falta h")?
            .parse()
            .map_err(|_| "h no es HOST:PORT")?;
        let server_pin = SpkiPin::from_hex(&pin.ok_or("falta pin")?).ok_or("pin inválido")?;
        let raw = from_hex(&token.ok_or("falta t")?)?;
        let bytes: [u8; 32] = raw.try_into().map_err(|_| "token de 32 bytes")?;
        Ok(Self {
            enroll_addr,
            server_pin,
            token: EnrollmentToken::from_bytes(bytes),
            session_port: None,
        })
    }

    fn session_port_or(&self, fallback: u16) -> u16 {
        self.session_port.unwrap_or(fallback)
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("hex impar".to_owned());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16).map_err(|_| "hex inválido".to_owned())
        })
        .collect()
}
