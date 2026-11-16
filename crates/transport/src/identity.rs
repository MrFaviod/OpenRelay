//! Identidad de dispositivo y validación de pares.
//!
//! La clave privada no vive aquí: el consumidor entrega un
//! [`rustls::sign::SigningKey`] (en Android, un puente JNI sobre Keystore; en
//! tests, una clave en memoria). Este módulo sólo valida que el certificado
//! corresponda a esa clave, calcula el pin SPKI del certificado presentado y
//! construye verificadores que exigen CA local **y** pin cuando corresponde.
//!
//! El DER se valida con el parser estándar `x509-cert` (RFC 5280, DER estricto);
//! no hay parser manual de TLV. Un certificado malformado es un error, nunca un
//! pin calculado sobre bytes arbitrarios.

use std::fmt;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{ResolvesClientCert, WebPkiServerVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::{CertifiedKey, SigningKey};
use rustls::{
    CertificateError, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};
use x509_cert::der::asn1::AnyRef;
use x509_cert::der::{self, Decode, Reader, SliceReader, Tag, Tagged};
use x509_cert::Certificate;

/// Certificados máximos aceptados en una cadena de identidad.
pub const MAX_CERT_CHAIN: usize = 4;
/// Bytes máximos de un certificado DER aceptado por el parser.
pub const MAX_CERT_DER: usize = 16 * 1024;

/// Error de identidad, sin material secreto en el mensaje.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdentityError {
    /// La cadena de certificados está vacía.
    EmptyChain,
    /// La cadena supera [`MAX_CERT_CHAIN`].
    ChainTooLong,
    /// Un certificado supera [`MAX_CERT_DER`].
    CertificateTooLarge,
    /// El DER no tiene la forma esperada.
    MalformedDer(&'static str),
    /// No se encontró un `SubjectPublicKeyInfo` válido.
    SpkiMissing,
    /// La clave entregada no corresponde al certificado hoja.
    KeyMismatch,
    /// rustls rechazó el almacén de anclas.
    RootStore,
    /// rustls no pudo construir el verificador webpki.
    Verifier,
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyChain => write!(f, "cadena de certificados vacía"),
            Self::ChainTooLong => write!(f, "cadena de certificados demasiado larga"),
            Self::CertificateTooLarge => write!(f, "certificado DER demasiado grande"),
            Self::MalformedDer(reason) => write!(f, "DER inválido: {reason}"),
            Self::SpkiMissing => write!(f, "certificado sin SubjectPublicKeyInfo"),
            Self::KeyMismatch => write!(f, "la clave no corresponde al certificado"),
            Self::RootStore => write!(f, "ancla de confianza rechazada"),
            Self::Verifier => write!(f, "no se pudo construir el verificador"),
        }
    }
}

impl std::error::Error for IdentityError {}

/// Pin SHA-256 del `SubjectPublicKeyInfo` de un certificado.
///
/// Es el valor que viaja en el QR de enrolamiento y el que indexa la ACL de
/// sesión. No es secreto, pero se imprime en hexadecimal y su `Debug` no
/// expone otra cosa.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpkiPin([u8; 32]);

impl SpkiPin {
    /// Calcula el pin del certificado DER entregado.
    pub fn from_certificate_der(cert: &[u8]) -> Result<Self, IdentityError> {
        let spki = extract_spki_der(cert)?;
        Ok(Self::from_spki_der(spki))
    }

    /// Calcula el pin de un `SubjectPublicKeyInfo` DER completo.
    pub fn from_spki_der(spki: &[u8]) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, spki);
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_ref());
        Self(out)
    }

    /// Bytes crudos del pin.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Inversa de [`Self::to_hex`]: 64 dígitos hexadecimales, o `None`.
    pub fn from_hex(text: &str) -> Option<Self> {
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
        Some(Self(out))
    }

    /// Representación hexadecimal minúscula de 64 caracteres.
    pub fn to_hex(self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            fmt::Write::write_fmt(&mut out, format_args!("{byte:02x}")).expect("String no falla");
        }
        out
    }
}

impl fmt::Debug for SpkiPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpkiPin({})", self.to_hex())
    }
}

impl fmt::Display for SpkiPin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Extrae el `SubjectPublicKeyInfo` DER completo de un certificado X.509.
///
/// La validación la hace el parser DER estándar de `x509-cert` (RFC 5280,
/// DER estricto: longitudes canónicas, sin bytes sobrantes, tipos correctos).
/// Después, el lector `der` recorre `tbsCertificate` sólo para localizar el
/// TLV del SPKI dentro de los bytes originales, saltando versión opcional,
/// serial, algoritmo de firma, emisor, validez y sujeto, en ese orden.
pub fn extract_spki_der(cert: &[u8]) -> Result<&[u8], IdentityError> {
    if cert.len() > MAX_CERT_DER {
        return Err(IdentityError::CertificateTooLarge);
    }
    Certificate::from_der(cert).map_err(der_error)?;

    let mut outer = SliceReader::new(cert).map_err(der_error)?;
    let cert_seq = outer.decode::<AnyRef<'_>>().map_err(der_error)?;
    let mut body = SliceReader::new(cert_seq.value()).map_err(der_error)?;
    let tbs_any = body.decode::<AnyRef<'_>>().map_err(der_error)?;
    if tbs_any.tag() != Tag::Sequence {
        return Err(IdentityError::MalformedDer("tbsCertificate no es SEQUENCE"));
    }
    let tbs = tbs_any.value();
    let mut t = SliceReader::new(tbs).map_err(der_error)?;

    // version [0] EXPLICIT, opcional.
    if t.peek_byte() == Some(0xa0) {
        t.decode::<AnyRef<'_>>().map_err(der_error)?;
    }
    // serialNumber, signature, issuer, validity, subject.
    for _ in 0..5 {
        t.decode::<AnyRef<'_>>().map_err(der_error)?;
    }
    let start = t.position();
    let spki = t.decode::<AnyRef<'_>>().map_err(der_error)?;
    if spki.tag() != Tag::Sequence {
        return Err(IdentityError::SpkiMissing);
    }
    let end = t.position();
    let tbs_off = offset_within(cert, tbs);
    let from = tbs_off + usize::try_from(start).map_err(|_| IdentityError::SpkiMissing)?;
    let to = tbs_off + usize::try_from(end).map_err(|_| IdentityError::SpkiMissing)?;
    Ok(&cert[from..to])
}

/// Desplazamiento de `inner` dentro de `outer`; ambos son subslices del mismo buffer.
fn offset_within(outer: &[u8], inner: &[u8]) -> usize {
    inner.as_ptr() as usize - outer.as_ptr() as usize
}

fn der_error(_: der::Error) -> IdentityError {
    IdentityError::MalformedDer("DER estricto rechazado")
}

/// Crea un [`CertifiedKey`] y exige que la clave pública declarada por el
/// signer coincida con el certificado hoja.
///
/// Esto evita configurar un signer que firme con una clave distinta de la
/// certificada, un fallo que de otro modo sólo aparecería como handshake roto.
pub fn certified_key(
    chain: Vec<CertificateDer<'static>>,
    key: Arc<dyn SigningKey>,
) -> Result<CertifiedKey, IdentityError> {
    if chain.is_empty() {
        return Err(IdentityError::EmptyChain);
    }
    if chain.len() > MAX_CERT_CHAIN {
        return Err(IdentityError::ChainTooLong);
    }
    for cert in &chain {
        extract_spki_der(cert.as_ref())?;
    }
    let certified = CertifiedKey::new(chain, key);
    certified
        .keys_match()
        .map_err(|_| IdentityError::KeyMismatch)?;
    Ok(certified)
}

/// Construye un almacén de anclas a partir de certificados DER.
pub fn root_store(
    certs: impl IntoIterator<Item = CertificateDer<'static>>,
) -> Result<Arc<RootCertStore>, IdentityError> {
    let mut roots = RootCertStore::empty();
    for cert in certs {
        roots.add(cert).map_err(|_| IdentityError::RootStore)?;
    }
    Ok(Arc::new(roots))
}

/// Verificador de servidor de sesión: webpki normal contra la CA local **y**
/// pin SPKI exacto del certificado hoja.
///
/// El pin se comprueba después de la cadena y la validez; un pin incorrecto o
/// un certificado caducado abortan el handshake antes de enviar datos.
#[derive(Debug)]
pub struct PinnedServerVerifier {
    inner: Arc<WebPkiServerVerifier>,
    pin: SpkiPin,
}

impl PinnedServerVerifier {
    /// Construye el verificador con las anclas y el pin esperado.
    pub fn new(roots: Arc<RootCertStore>, pin: SpkiPin) -> Result<Self, IdentityError> {
        let inner = WebPkiServerVerifier::builder(roots)
            .build()
            .map_err(|_| IdentityError::Verifier)?;
        Ok(Self { inner, pin })
    }
}

impl ServerCertVerifier for PinnedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        let actual = SpkiPin::from_certificate_der(end_entity.as_ref())
            .map_err(|_| TlsError::InvalidCertificate(CertificateError::BadEncoding))?;
        if actual != self.pin {
            return Err(TlsError::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Resolvedor de servidor con un único [`CertifiedKey`] estático.
#[derive(Debug)]
pub struct StaticServerCertResolver {
    key: Arc<CertifiedKey>,
}

impl StaticServerCertResolver {
    /// Envuelve la identidad del daemon.
    pub fn new(key: Arc<CertifiedKey>) -> Self {
        Self { key }
    }
}

impl ResolvesServerCert for StaticServerCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.key))
    }
}

/// Resolvedor de cliente con una única identidad estática.
#[derive(Debug)]
pub struct StaticClientCertResolver {
    key: Arc<CertifiedKey>,
}

impl StaticClientCertResolver {
    /// Envuelve la identidad del cliente.
    pub fn new(key: Arc<CertifiedKey>) -> Self {
        Self { key }
    }
}

impl ResolvesClientCert for StaticClientCertResolver {
    fn resolve(
        &self,
        _root_hint_subjects: &[&[u8]],
        _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.key))
    }

    fn has_certs(&self) -> bool {
        true
    }
}
