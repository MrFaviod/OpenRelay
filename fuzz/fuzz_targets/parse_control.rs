#![no_main]

//! Fuzz del decoder de control `openrelay.control/v1`.
//!
//! Alimenta la puerta semántica real de `openrelay-proto`: prefijo `u32` BE, JSON
//! estricto acotado y mensaje tipado. No reimplementa el parser.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = openrelay_proto::wire::decode_message(data);
});
