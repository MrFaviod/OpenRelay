//! Aprovisionamiento local del daemon: una CA propia, el certificado del
//! servidor y los archivos de estado que antes aportaba el operador a mano.
//!
//! Se generan sólo los archivos que faltan, con escritura atómica (temporal +
//! `rename`) y modo 0600. La CA local firma tanto el servidor de sesión como
//! las hojas de cliente que emite el emparejamiento; por eso el cliente del
//! teléfono recibe `client-ca.der` como raíz de sesión.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rcgen::{
    date_time_ymd, BasicConstraints, Certificate, CertificateParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};

use crate::runtime::ProvisionFiles;

/// Nombre del directorio bajo la config del usuario.
pub const STATE_DIR_NAME: &str = "openrelay";
/// `CommonName` de la CA local. `issue::load` lo reconstruye desde el DER.
pub const CA_COMMON_NAME: &str = "openrelay-ca";

/// `${XDG_CONFIG_HOME:-~/.config}/openrelay`.
pub fn default_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join(STATE_DIR_NAME));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join(STATE_DIR_NAME))
}

/// Crea el directorio y genera los archivos que falten.
///
/// Devuelve las rutas que `runtime::start` consume. Dos pares son atómicos por
/// necesidad criptográfica: si falta un elemento del par CA o servidor, se
/// regenera el par completo para no dejar una clave que no corresponde a su
/// certificado.
pub fn ensure(dir: &Path) -> io::Result<ProvisionFiles> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

    let ca_cert = dir.join("client-ca.der");
    let ca_key = dir.join("client-ca.key.der");
    if !ca_cert.exists() || !ca_key.exists() {
        let (cert, key) = generate_ca().map_err(config_error)?;
        write_atomic(&ca_cert, cert.der(), 0o600)?;
        write_atomic(&ca_key, &key.serialize_der(), 0o600)?;
    }

    let server_cert = dir.join("server.der");
    let server_key = dir.join("server.key.der");
    if !server_cert.exists() || !server_key.exists() {
        let ca = load_ca(&ca_cert, &ca_key)?;
        let (cert, key) = generate_server(&ca).map_err(config_error)?;
        write_atomic(&server_cert, cert.der(), 0o600)?;
        write_atomic(&server_key, &key.serialize_der(), 0o600)?;
    }

    let identities = dir.join("identities.json");
    write_atomic_if_missing(&identities, b"{\"version\":1,\"identities\":[]}\n")?;
    let control_grants = dir.join("grants.txt");
    write_atomic_if_missing(&control_grants, b"")?;

    Ok(ProvisionFiles {
        identity_der: server_cert,
        identity_key: server_key,
        client_ca_der: ca_cert,
        client_ca_key: ca_key,
        identities,
        control_grants,
    })
}

/// Carga la CA para firmar hojas de cliente.
fn load_ca(cert_path: &Path, key_path: &Path) -> io::Result<crate::issue::LocalCa> {
    crate::issue::load(cert_path, key_path).map_err(io::Error::other)
}

fn generate_ca() -> Result<(Certificate, KeyPair), rcgen::Error> {
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params
        .distinguished_name
        .push(DnType::CommonName, CA_COMMON_NAME);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert, key))
}

fn generate_server(ca: &crate::issue::LocalCa) -> Result<(Certificate, KeyPair), rcgen::Error> {
    let mut params = CertificateParams::new(vec![
        "localhost".to_string(),
        "openrelay".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])?;
    params
        .distinguished_name
        .push(DnType::CommonName, "openrelay-server");
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.not_before = date_time_ymd(1975, 1, 1);
    params.not_after = date_time_ymd(4096, 1, 1);
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, ca.cert(), ca.key())?;
    Ok((cert, key))
}

fn write_atomic_if_missing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    write_atomic(path, bytes, 0o600)
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("openrelay");
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn config_error(error: rcgen::Error) -> io::Error {
    io::Error::other(format!("certificate generation: {error}"))
}
