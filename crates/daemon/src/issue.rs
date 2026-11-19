//! Emisión de hojas de cliente desde el SPKI que declaró el teléfono.
//!
//! La clave privada del dispositivo nunca llega al daemon: sólo su SPKI. Se
//! construye una `RemoteKeyPair` con el punto P-256 sin comprimir y rcgen
//! rellena el TBS con esa clave pública; la firma la pone la CA local.

use std::fs;
use std::path::Path;

use rcgen::{
    date_time_ymd, Certificate, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, RemoteKeyPair,
};

use crate::provision::CA_COMMON_NAME;

/// Prefijo SPKI de P-256 sin comprimir (verificado contra rcgen 0.13.1).
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];
const SPKI_LEN: usize = 91;
const POINT_OFFSET: usize = 26;

/// CA local lista para firmar. El `Certificate` reconstruido sólo aporta el
/// `subject` y la clave pública del emisor a rcgen.
pub struct LocalCa {
    cert: Certificate,
    key: KeyPair,
}

impl LocalCa {
    pub fn cert(&self) -> &Certificate {
        &self.cert
    }

    pub fn key(&self) -> &KeyPair {
        &self.key
    }
}

/// Carga `client-ca.der` y su clave PKCS#8 DER.
pub fn load(cert_path: &Path, key_path: &Path) -> Result<LocalCa, String> {
    let cert_der =
        fs::read(cert_path).map_err(|error| format!("CA ({}): {error}", cert_path.display()))?;
    let key_der = fs::read(key_path)
        .map_err(|error| format!("clave CA ({}): {error}", key_path.display()))?;
    let key = KeyPair::try_from(key_der.as_slice())
        .map_err(|error| format!("clave CA no es PKCS#8: {error}"))?;
    // Los parámetros reproducen el `subject` con el que `provision` generó la
    // CA; `signed_by` sólo usa ese DN y la clave para el campo `issuer`.
    let mut params = CertificateParams::new(Vec::<String>::new())
        .map_err(|error| format!("parámetros CA: {error}"))?;
    params
        .distinguished_name
        .push(DnType::CommonName, CA_COMMON_NAME);
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    let cert = params
        .self_signed(&key)
        .map_err(|error| format!("CA reconstruida: {error}"))?;
    // La firma ECDSA es aleatoria, así que el DER completo nunca coincide al
    // re-firmar. Se compara el TBS: ahí viven sujeto, validez, serial y SPKI.
    if tbs_der(cert.der().as_ref()) != tbs_der(&cert_der) {
        return Err(format!(
            "la CA ({}) no fue generada por openrelay o cambió su formato",
            cert_path.display()
        ));
    }
    Ok(LocalCa { cert, key })
}

/// Bytes del `tbsCertificate` (primer hijo del SEQUENCE exterior).
fn tbs_der(der: &[u8]) -> Option<&[u8]> {
    let (length, header) = der_length(der.get(1..)?)?;
    if der.first() != Some(&0x30) || 1 + header + length != der.len() {
        return None;
    }
    let body = &der[1 + header..];
    let (inner, inner_header) = der_length(body.get(1..)?)?;
    let end = 1 + inner_header + inner;
    (body.first() == Some(&0x30) && end <= body.len()).then(|| &body[..end])
}

/// Longitud DER y bytes de cabecera (sin el tag) para longitudes cortas y
/// largas de hasta tres bytes, suficiente para un certificado X.509.
fn der_length(bytes: &[u8]) -> Option<(usize, usize)> {
    let first = *bytes.first()?;
    if first < 0x80 {
        return Some((first as usize, 1));
    }
    let count = (first & 0x7f) as usize;
    if !(1..=3).contains(&count) || bytes.len() < 1 + count {
        return None;
    }
    let mut length = 0usize;
    for byte in &bytes[1..1 + count] {
        length = (length << 8) | *byte as usize;
    }
    Some((length, 1 + count))
}

/// Firma una hoja `clientAuth` cuyo SPKI es exactamente `spki_der`.
pub fn issue_client_leaf(ca: &LocalCa, spki_der: &[u8]) -> Result<Vec<u8>, String> {
    let point = p256_point(spki_der)?;
    let mut params =
        CertificateParams::new(Vec::<String>::new()).map_err(|error| error.to_string())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "openrelay-device");
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    let key = KeyPair::from_remote(Box::new(PublicOnlyKey { point }))
        .map_err(|error| format!("SPKI remoto: {error}"))?;
    let cert = params
        .signed_by(&key, &ca.cert, &ca.key)
        .map_err(|error| format!("hoja firmada: {error}"))?;
    Ok(cert.der().as_ref().to_vec())
}

fn p256_point(spki: &[u8]) -> Result<Vec<u8>, String> {
    if spki.len() != SPKI_LEN || spki[..POINT_OFFSET] != P256_SPKI_PREFIX {
        return Err(format!(
            "SPKI no es P-256 sin comprimir ({} bytes)",
            spki.len()
        ));
    }
    if spki[POINT_OFFSET] != 0x04 {
        return Err("punto P-256 comprimido o inválido".into());
    }
    Ok(spki[POINT_OFFSET..].to_vec())
}

/// Clave pública sin privada. `sign` no se usa: la firma del TBS la pone la CA.
struct PublicOnlyKey {
    point: Vec<u8>,
}

impl RemoteKeyPair for PublicOnlyKey {
    fn public_key(&self) -> &[u8] {
        &self.point
    }

    fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        Err(rcgen::Error::RingUnspecified)
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}
