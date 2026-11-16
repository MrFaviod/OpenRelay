//! Puente JNI de test (F2-10c): handshake mTLS real con la clave de AndroidKeyStore.
//!
//! La clave privada nunca sale de Java: `KeystoreSigner.sign` firma por JNI.
//! `unsafe` sólo aparece como permiso del lint en los dos atributos de export.

#![deny(unsafe_code)]

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use arl_transport::identity::{self, SpkiPin};
use arl_transport::quinn::{
    Acl, Capabilities, RejectReason, ServerEvent, SessionClient, SessionLimits,
    SessionServer,
};
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JValue, JValueOwned};
use jni::{JNIEnv, JavaVM};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, RemoteKeyPair, PKCS_ECDSA_P256_SHA256,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, SubjectPublicKeyInfoDer};
use rustls::sign::{CertifiedKey, Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};

const SERVER_ADDR: std::net::SocketAddr =
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 44331);
const CLIENT_ADDR: std::net::SocketAddr =
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 44332);

fn text<E: fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Estado compartido con el Keystore: VM, referencia global al `KeystoreSigner`
/// y contador de firmas TLS hechas por Java.
impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Inner")
    }
}

struct Inner {
    vm: JavaVM,
    signer: GlobalRef,
    signatures: AtomicUsize,
}

impl Inner {
    /// Llama a `KeystoreSigner.sign([B)[B`; devuelve la firma DER de Java.
    fn sign_java(&self, msg: &[u8]) -> Result<Vec<u8>, String> {
        let mut env = self.vm.attach_current_thread().map_err(text)?;
        let arr = env.byte_array_from_slice(msg).map_err(text)?;
        let out = env
            .call_method(
                self.signer.as_obj(),
                "sign",
                "([B)[B",
                &[JValue::Object(&arr)],
            )
            .map_err(text)?
            .l()
            .map_err(text)?;
        env.convert_byte_array(JByteArray::from(out)).map_err(text)
    }
}

/// Clave pública del Keystore como `SigningKey` de rustls (ECDSA P-256).
struct KeystoreKey {
    inner: Arc<Inner>,
    spki: Vec<u8>,
}

impl fmt::Debug for KeystoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeystoreKey")
    }
}

impl SigningKey for KeystoreKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        if !offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256) {
            return None;
        }
        Some(Box::new(KeystoreSignerHandle {
            inner: Arc::clone(&self.inner),
        }))
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(SubjectPublicKeyInfoDer::from(self.spki.as_slice()))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

#[derive(Debug)]
struct KeystoreSignerHandle {
    inner: Arc<Inner>,
}

impl Signer for KeystoreSignerHandle {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let sig = self
            .inner
            .sign_java(message)
            .map_err(|e| rustls::Error::General(format!("keystore: {e}")))?;
        self.inner.signatures.fetch_add(1, Ordering::SeqCst);
        Ok(sig)
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

/// Adaptador para que rcgen tome la clave pública del Keystore. rcgen no firma
/// con esta clave (el emisor es la CA de test); `sign` queda cableado igual.
struct KeystoreRemote {
    inner: Arc<Inner>,
    raw_point: Vec<u8>,
}

impl RemoteKeyPair for KeystoreRemote {
    fn public_key(&self) -> &[u8] {
        &self.raw_point
    }

    fn sign(&self, msg: &[u8]) -> Result<Vec<u8>, rcgen::Error> {
        self.inner.sign_java(msg).map_err(|_| rcgen::Error::RingUnspecified)
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        &PKCS_ECDSA_P256_SHA256
    }
}

/// Punto P-256 sin comprimir (65 bytes) al final del SPKI de 91 bytes.
fn raw_point(spki: &[u8]) -> Result<Vec<u8>, String> {
    if spki.len() != 91 || spki[26] != 0x04 {
        return Err("SPKI del Keystore no es P-256 sin comprimir".into());
    }
    Ok(spki[26..].to_vec())
}

fn ca_params() -> CertificateParams {
    let mut p = CertificateParams::new(Vec::<String>::new()).expect("params CA");
    p.distinguished_name.push(DnType::CommonName, "arl-test-ca");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    p
}

fn leaf_params(cn: &str, server: bool) -> CertificateParams {
    let mut p = CertificateParams::new(vec!["localhost".to_string()]).expect("params hoja");
    p.distinguished_name.push(DnType::CommonName, cn);
    p.is_ca = IsCa::ExplicitNoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![if server {
        ExtendedKeyUsagePurpose::ServerAuth
    } else {
        ExtendedKeyUsagePurpose::ClientAuth
    }];
    p
}

/// Pump en memoria entre dos peers sans-IO (mismo patrón que tests/common).
fn pump(client: &mut SessionClient, server: &mut SessionServer) -> Result<(), String> {
    let now = Instant::now();
    for _ in 0..24 {
        for _ in 0..256 {
            let Some((_, d)) = client.poll_transmit(now) else { break };
            server.handle_datagram(now, CLIENT_ADDR, &d).map_err(text)?;
        }
        for _ in 0..256 {
            let Some((_, d)) = server.poll_transmit(now) else { break };
            client.handle_datagram(now, SERVER_ADDR, &d).map_err(text)?;
        }
    }
    Ok(())
}

/// Un handshake completo; devuelve eventos del servidor y si el cliente quedó autorizado.
fn session(
    client_id: Arc<CertifiedKey>,
    server_id: Arc<CertifiedKey>,
    ca_der: &CertificateDer<'static>,
    server_pin: SpkiPin,
    acl: Acl,
) -> Result<(Vec<ServerEvent>, bool), String> {
    let roots = identity::root_store([ca_der.clone()]).map_err(text)?;
    let mut server = SessionServer::new(server_id, roots.clone(), acl, SessionLimits::default())
        .map_err(text)?;
    let mut client = SessionClient::connect(
        client_id,
        roots,
        server_pin,
        SessionLimits::default(),
        Instant::now(),
        SERVER_ADDR,
        "localhost",
    )
    .map_err(text)?;
    pump(&mut client, &mut server)?;
    let mut events = Vec::new();
    while let Some(ev) = server.next_event() {
        events.push(ev);
    }
    Ok((events, client.is_authorized()))
}

fn run(env: &mut JNIEnv, signer: JObject) -> Result<String, String> {
    let vm = env.get_java_vm().map_err(text)?;
    let signer_ref = env.new_global_ref(&signer).map_err(text)?;
    let spki = bytes_of(
        env.call_method(&signer, "publicKeyDer", "()[B", &[])
            .map_err(text)?,
        env,
    )?;
    let inner = Arc::new(Inner {
        vm,
        signer: signer_ref,
        signatures: AtomicUsize::new(0),
    });

    // CA de test (software) y hoja del cliente con la clave del Keystore.
    let ca_kp = KeyPair::generate().map_err(text)?;
    let ca = ca_params().self_signed(&ca_kp).map_err(text)?;
    let client_kp = KeyPair::from_remote(Box::new(KeystoreRemote {
        inner: Arc::clone(&inner),
        raw_point: raw_point(&spki)?,
    }))
    .map_err(text)?;
    let client_leaf = leaf_params("arl-android-keystore", false)
        .signed_by(&client_kp, &ca, &ca_kp)
        .map_err(text)?;
    let client_der = client_leaf.der().clone();
    let ca_der = ca.der().clone();
    let client_pin = SpkiPin::from_certificate_der(&client_der).map_err(text)?;

    let keystore_key = Arc::new(KeystoreKey {
        inner: Arc::clone(&inner),
        spki,
    });
    let client_id = Arc::new(
        identity::certified_key(vec![client_der.clone(), ca_der.clone()], keystore_key)
            .map_err(text)?,
    );

    // Servidor: clave software, pin del cliente sólo en la ACL.
    let server_kp = KeyPair::generate().map_err(text)?;
    let server_cert = leaf_params("arl-daemon", true)
        .signed_by(&server_kp, &ca, &ca_kp)
        .map_err(text)?;
    let server_sk = any_ecdsa_type(&PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        server_kp.serialize_der(),
    )))
    .map_err(text)?;
    let server_id = Arc::new(
        identity::certified_key(vec![server_cert.der().clone(), ca_der.clone()], server_sk)
            .map_err(text)?,
    );
    let server_pin = SpkiPin::from_certificate_der(server_cert.der()).map_err(text)?;
    let caps = Capabilities::none()
        .with_mic()
        .with_control_read()
        .with_control_write();

    // Positivo: el pin del Keystore está en la ACL → debe autorizar.
    let mut acl = Acl::new();
    acl.allow(client_pin, caps);
    let before = inner.signatures.load(Ordering::SeqCst);
    let (events, client_ok) = session(
        Arc::clone(&client_id),
        Arc::clone(&server_id),
        &ca_der,
        server_pin,
        acl,
    )?;
    let sigs_pos = inner.signatures.load(Ordering::SeqCst) - before;
    let authorized = events
        .iter()
        .any(|e| matches!(e, ServerEvent::Authorized { identity, .. } if *identity == client_pin));
    if !authorized {
        return Err(format!("servidor no autorizó: {events:?}"));
    }
    if !client_ok {
        return Err("cliente no validó el servidor".into());
    }
    if sigs_pos == 0 {
        return Err("el Keystore no firmó ninguna vez".into());
    }

    // Negativo: la ACL sólo tiene el pin de otra identidad → no autoriza.
    let other_kp = KeyPair::generate().map_err(text)?;
    let other_pin = SpkiPin::from_spki_der(&other_kp.public_key_der());
    let mut wrong = Acl::new();
    wrong.allow(other_pin, caps);
    let before_neg = inner.signatures.load(Ordering::SeqCst);
    let (neg_events, _) = session(client_id, server_id, &ca_der, server_pin, wrong)?;
    let sigs_neg = inner.signatures.load(Ordering::SeqCst) - before_neg;
    if neg_events
        .iter()
        .any(|e| matches!(e, ServerEvent::Authorized { .. }))
    {
        return Err("negativo: el servidor autorizó con pin ajeno".into());
    }
    let reason = neg_events
        .iter()
        .find_map(|e| match e {
            ServerEvent::Rejected { reason } => Some(*reason),
            _ => None,
        })
        .ok_or("negativo: sin evento Rejected")?;
    if reason != RejectReason::NoAclEntry {
        return Err(format!("negativo: motivo inesperado {reason:?}"));
    }

    Ok(format!(
        "OK sigs_pos={sigs_pos} sigs_neg={sigs_neg} pin={client_pin} neg_reject={reason:?}"
    ))
}

fn bytes_of(value: JValueOwned, env: &JNIEnv) -> Result<Vec<u8>, String> {
    let obj = value.l().map_err(text)?;
    env.convert_byte_array(JByteArray::from(obj)).map_err(text)
}

mod exports {
    use super::*;

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_arl_audio_KeystoreHandshakeTest_nativeHandshake(
        mut env: JNIEnv,
        _class: JClass,
        signer: JObject,
    ) -> jni::sys::jstring {
        let report = run(&mut env, signer).unwrap_or_else(|e| format!("ERR {e}"));
        match env.new_string(report) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    }
}
