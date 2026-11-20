//! `openrelay-proto` — subconjunto de control del contrato `openrelay.control/v1`.
//!
//! Alcance: envelope, framing de control (`u32` BE + JSON acotado),
//! máquina de estados de configuración (revisión, `request_id`,
//! `expected_revision`, conflicto, ACK efectivo, capacidades y catálogo de
//! errores) e idempotencia por sesión.
//!
//! Fuera de alcance, por contrato: sockets, TLS, criptografía, códec y
//! cualquier transporte. El framing de audio (28 bytes de cabecera + PCM16LE,
//! 508 bytes por datagrama) vive en [`docs/protocol.md`](../../../docs/protocol.md)
//! §8 y en `contracts/audio-frame-v1.schema.json`; esta crate **no** lo
//! redefine ni lo implementa.
//!
//! La autoridad de configuración es el worker de control del daemon: un único
//! hilo aplica las operaciones en orden. Esta crate modela ese worker; no
//! autentica ni autoriza por sí misma: recibe un [`control::AuthorizedPeer`]
//! ya autenticado por la frontera de transporte.

pub mod control;
pub mod wire;
