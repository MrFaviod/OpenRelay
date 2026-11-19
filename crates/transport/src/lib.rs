//! Transporte de arl: QUIC real sobre `quinn-proto` (sans-IO) y `rustls`.
//!
//! Este crate implementa dos superficies **disjuntas**:
//!
//! - [`enrollment`] — ALPN `arl-enroll/1`, servidor con certificado efímero y
//!   pin SPKI en el QR, **sin** certificado de cliente, sin DATAGRAM, sin
//!   tickets y sin 0-RTT. Sólo mensajes de enrolamiento; audio, control y
//!   tickets se rechazan y cierran.
//! - [`quinn`] — ALPN `arl-session/1`, mTLS obligatorio, cadena contra la CA
//!   local, pin SPKI adicional del servidor, identidad de peer explícita y ACL
//!   *default deny* antes de entregar DATAGRAM de audio o streams de control.
//!
//! La identidad del dispositivo se construye con un [`rustls::sign::SigningKey`]
//! externo: el material privado nunca entra en este crate. [`identity`] expone
//! el pin SPKI (SHA-256 del `SubjectPublicKeyInfo`), el parser DER y los
//! verificadores; el puente Android Keystore sigue siendo trabajo de F2/T10c.
//!
//! Límites deliberados de T10:
//!
//! - El crate es sans-IO. El llamador entrega y recibe datagramas; los tests
//!   usan un enlace en memoria con paquetes QUIC reales. El socket UDP de
//!   producción y el JNI se cablean en T13/T14.
//! - No hay tickets de sesión ni 0-RTT: la autorización se evalúa después del
//!   handshake y antes de leer cualquier dato de aplicación.
//! - `G3` (Keystore real), `G4` (NDK) y `G5` (LAN) **no** quedan probados por
//!   este crate ni por sus tests.
//!
//! El frame de audio es el de [`arl_audio_core`] y el control usa el framing de
//! [`arl_proto::wire`]; este crate no redefine ninguno de los dos.

#![forbid(unsafe_code)]

pub mod enrollment;
pub mod identity;
pub mod quinn;
pub(crate) mod rate_limit;

/// ALPN exacto de la superficie de enrolamiento.
pub const ALPN_ENROLL: &[u8] = b"arl-enroll/1";
/// ALPN exacto de la superficie de sesión.
pub const ALPN_SESSION: &[u8] = b"arl-session/1";

/// Tamaño máximo de un frame de control en el wire (prefijo incluido).
pub const MAX_CONTROL_FRAME: usize =
    arl_proto::wire::MAX_CONTROL_PAYLOAD_BYTES + arl_proto::wire::CONTROL_LENGTH_BYTES;
/// Tamaño máximo de un frame de audio v1.
pub const MAX_AUDIO_DATAGRAM: usize = arl_audio_core::MAX_DATAGRAM_LEN;
