//! `arl service print-unit` imprime la unidad empaquetada, byte por byte.
//!
//! La fuente es `packaging/systemd/arl.service`; si el archivo y el binario se
//! separan, el empaquetado deja de ser reproducible y este test lo detecta.

use std::process::Command;

const ARL: &str = env!("CARGO_BIN_EXE_arl");
const UNIT: &str = include_str!("../../../packaging/systemd/arl.service");

#[test]
fn print_unit_matches_packaged_file() {
    let out = Command::new(ARL)
        .args(["service", "print-unit"])
        .output()
        .expect("ejecutar arl service print-unit");
    assert!(out.status.success(), "salió con {}", out.status);
    assert_eq!(out.stdout, UNIT.as_bytes());
    assert!(
        out.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn service_without_subcommand_is_usage_error() {
    let out = Command::new(ARL)
        .arg("service")
        .output()
        .expect("ejecutar arl service");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[test]
fn service_help_succeeds_and_documents_print_unit() {
    let out = Command::new(ARL)
        .args(["service", "--help"])
        .output()
        .expect("ejecutar arl service --help");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("print-unit"));
}
