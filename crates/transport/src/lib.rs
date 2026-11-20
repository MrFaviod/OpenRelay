//! Transporte de openrelay: QUIC real sobre `quinn-proto` (sans-IO) y `rustls`.
//!
//! Este crate implementa dos superficies **disjuntas**:
//!
//! - [`enrollment`] — ALPN `openrelay-enroll/1`, servidor con certificado efímero y
//!   pin SPKI en el QR, **sin** certificado de cliente, sin DATAGRAM, sin
//!   tickets y sin 0-RTT. Sólo mensajes de enrolamiento; audio, control y
//!   tickets se rechazan y cierran.
//! - [`quinn`] — ALPN `openrelay-session/1`, mTLS obligatorio, cadena contra la CA
//!   local, pin SPKI adicional del servidor, identidad de peer explícita y ACL
//!   *default deny* antes de entregar DATAGRAM de audio o streams de control.
//!
//! La identidad del dispositivo se construye con un [`rustls::sign::SigningKey`]
//! externo: el material privado nunca entra en este crate. [`identity`] expone
//! el pin SPKI (SHA-256 del `SubjectPublicKeyInfo`), el parser DER y los
//! verificadores; el puente Android Keystore queda fuera de esta crate.
//!
//! Límites deliberados:
//!
//! - El crate es sans-IO. El llamador entrega y recibe datagramas; los tests
//!   usan un enlace en memoria con paquetes QUIC reales. El socket UDP de
//!   producción y el JNI se cablean fuera de esta crate.
//! - No hay tickets de sesión ni 0-RTT: la autorización se evalúa después del
//!   handshake y antes de leer cualquier dato de aplicación.
//! - El Keystore real, el NDK y la LAN **no** quedan probados por
//!   este crate ni por sus tests.
//!
//! El frame de audio es el de [`openrelay_audio_core`] y el control usa el framing de
//! [`openrelay_proto::wire`]; este crate no redefine ninguno de los dos.

#![forbid(unsafe_code)]

pub mod enrollment;
pub mod identity;
pub mod quinn;
pub(crate) mod rate_limit;

/// ALPN exacto de la superficie de enrolamiento.
pub const ALPN_ENROLL: &[u8] = b"openrelay-enroll/1";
/// ALPN exacto de la superficie de sesión.
pub const ALPN_SESSION: &[u8] = b"openrelay-session/1";

/// Tamaño máximo de un frame de control en el wire (prefijo incluido).
pub const MAX_CONTROL_FRAME: usize =
    openrelay_proto::wire::MAX_CONTROL_PAYLOAD_BYTES + openrelay_proto::wire::CONTROL_LENGTH_BYTES;
/// Tamaño máximo de un frame de audio v1.
pub const MAX_AUDIO_DATAGRAM: usize = openrelay_audio_core::MAX_DATAGRAM_LEN;
