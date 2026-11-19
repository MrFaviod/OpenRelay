#![no_main]

//! Fuzz del decoder de control `arl.control/v1`.
//!
//! Alimenta la puerta semántica real de `arl-proto`: prefijo `u32` BE, JSON
//! estricto acotado y mensaje tipado. No reimplementa el parser.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = arl_proto::wire::decode_message(data);
});
