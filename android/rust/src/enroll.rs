//! Enrolamiento puntual desde el QR: una conexión QUIC al endpoint `enroll`
//! del PC, token de un solo uso y espera de la aprobación local.
//!
//! El cliente no guarda nada: entrega hoja, CA de sesión y puerto a Kotlin,
//! que es quien persiste la provisión. La clave privada sigue en el Keystore:
//! aquí sólo viaja su SPKI DER.

use std::net::SocketAddr;
use std::time::Instant;

use openrelay_transport::enrollment::{EnrollClient, EnrollmentToken};
use openrelay_transport::identity::{self, SpkiPin};
use rustls_pki_types::CertificateDer;

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
/// El QR sólo lleva el pin SPKI del certificado persistente del daemon, sin
/// anclas de cadena. [`EnrollClient::send_token`] bombea el handshake y el
/// resultado por su propio socket; el verificador del transporte autentica
/// sólo por pin cuando el almacén de anclas está vacío.
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
    let outcome = client
        .send_token(&token, &spki_der)
        .map_err(|error| format!("token: {error}"))?;
    Ok(Enrolled {
        leaf_der: outcome.client_leaf_der,
        ca_der: outcome.session_ca_der,
        port: outcome.session_port,
    })
}
