//! Enrolamiento puntual desde el QR: una conexión QUIC al endpoint `enroll`
//! del PC, token de un solo uso y espera de la aprobación local.
//!
//! El cliente no guarda nada: entrega hoja, CA de sesión y puerto a Kotlin,
//! que es quien persiste la provisión. La clave privada sigue en el Keystore:
//! aquí sólo viaja su SPKI DER.

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use openrelay_transport::enrollment::{EnrollClient, EnrollmentEvent, EnrollmentToken};
use openrelay_transport::identity::{self, SpkiPin};
use rustls_pki_types::CertificateDer;

/// Tope de la espera completa (handshake + aprobación humana en el PC).
const ENROLL_TIMEOUT: Duration = Duration::from_secs(150);
/// Sondeo del socket: sin esto el bucle no puede bombear el sans-IO.
const RECV_SLICE: Duration = Duration::from_millis(50);

pub(crate) struct Enrolled {
    pub leaf_der: Vec<u8>,
    pub ca_der: Vec<u8>,
    pub port: u16,
}

fn hex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let high = (bytes[index * 2] as char).to_digit(16)?;
        let low = (bytes[index * 2 + 1] as char).to_digit(16)?;
        *slot = ((high << 4) | low) as u8;
    }
    Some(out)
}

/// Conecta, espera `Ready`, envía el token y devuelve la provisión.
///
/// `roots` es la limitación conocida del contrato: el QR sólo lleva el pin
/// del certificado efímero, así que el ancla de cadena no está disponible en
/// el primer emparejamiento. Cuando el transporte exponga un `connect` que
/// autentique sólo por pin, esta función es el único punto que cambia.
pub(crate) fn enroll(
    host: &str,
    port: u16,
    pin_hex: &str,
    token_hex: &str,
    spki_der: Vec<u8>,
) -> Result<Enrolled, String> {
    let endpoint = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let addr: SocketAddr = endpoint
        .parse()
        .map_err(|_| "host:puerto del QR no válido".to_string())?;
    let pin = SpkiPin::from_hex(pin_hex).ok_or("pin del QR no es 64 hex")?;
    let token = hex32(token_hex)
        .map(EnrollmentToken::from_bytes)
        .ok_or("token del QR no es 64 hex")?;
    if spki_der.is_empty() {
        return Err("SPKI del cliente vacío".into());
    }
    let roots = identity::root_store(std::iter::empty::<CertificateDer<'static>>())
        .map_err(|error| error.to_string())?;
    let mut client = EnrollClient::connect(roots, pin, Instant::now(), addr, host)
        .map_err(|error| format!("conexión de enrolamiento: {error}"))?;
    let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|error| error.to_string())?;
    socket.connect(addr).map_err(|error| error.to_string())?;
    socket
        .set_read_timeout(Some(RECV_SLICE))
        .map_err(|error| error.to_string())?;

    let deadline = Instant::now() + ENROLL_TIMEOUT;
    let mut buf = [0u8; 4096];
    let mut ready = false;
    while !ready {
        if Instant::now() > deadline {
            return Err("el PC no completó el handshake de enrolamiento".into());
        }
        let now = Instant::now();
        while let Some((_destination, payload)) = client.poll_transmit(now) {
            socket.send(&payload).map_err(|error| error.to_string())?;
        }
        match socket.recv(&mut buf) {
            Ok(n) => client
                .handle_datagram(Instant::now(), addr, &buf[..n])
                .map_err(|error| error.to_string())?,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => return Err(format!("socket de enrolamiento: {error}")),
        }
        while let Some(event) = client.next_event() {
            match event {
                EnrollmentEvent::Ready => ready = true,
                EnrollmentEvent::Rejected(reason) => {
                    return Err(format!("token rechazado: {reason}"))
                }
                EnrollmentEvent::Closed => {
                    return Err("el PC cerró el enrolamiento".into())
                }
                EnrollmentEvent::Accepted(_) | EnrollmentEvent::ForbiddenSurface(_) => {}
            }
        }
    }

    // Último vuelo del handshake antes de encolar el token.
    let now = Instant::now();
    while let Some((_destination, payload)) = client.poll_transmit(now) {
        socket.send(&payload).map_err(|error| error.to_string())?;
    }
    send_token(&mut client, &token, &spki_der, &socket, addr, deadline)
}

#[cfg(feature = "enroll-outcome")]
fn send_token(
    client: &mut EnrollClient,
    token: &EnrollmentToken,
    spki_der: &[u8],
    _socket: &UdpSocket,
    _addr: SocketAddr,
    _deadline: Instant,
) -> Result<Enrolled, String> {
    let outcome = client
        .send_token(token, spki_der)
        .map_err(|error| format!("token: {error}"))?;
    Ok(Enrolled {
        leaf_der: outcome.client_leaf_der,
        ca_der: outcome.session_ca_der,
        port: outcome.session_port,
    })
}

#[cfg(not(feature = "enroll-outcome"))]
fn send_token(
    _client: &mut EnrollClient,
    _token: &EnrollmentToken,
    _spki_der: &[u8],
    _socket: &UdpSocket,
    _addr: SocketAddr,
    _deadline: Instant,
) -> Result<Enrolled, String> {
    Err("este build del puente no tiene EnrollOutcome: recompila android/rust cuando el transporte lo exponga".into())
}
