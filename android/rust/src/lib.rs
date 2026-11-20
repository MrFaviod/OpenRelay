//! Puente JNI de test: handshake mTLS real con la clave de AndroidKeyStore.
//!
//! La clave privada nunca sale de Java: `KeystoreSigner.sign` firma por JNI.
//! `unsafe` sólo aparece como permiso del lint en los dos atributos de export.

#![deny(unsafe_code)]

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use openrelay_transport::identity::{self, SpkiPin};
use openrelay_transport::quinn::{
    Acl, Capabilities, RejectReason, ServerEvent, SessionClient, SessionLimits, SessionServer,
};
use jni::objects::{GlobalRef, JByteArray, JClass, JObject, JShortArray, JValue, JValueOwned};
use jni::{JNIEnv, JavaVM};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, RemoteKeyPair, PKCS_ECDSA_P256_SHA256,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, SubjectPublicKeyInfoDer,
};
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
        self.inner
            .sign_java(msg)
            .map_err(|_| rcgen::Error::RingUnspecified)
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
    p.distinguished_name.push(DnType::CommonName, "openrelay-test-ca");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
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
            let Some((_, d)) = client.poll_transmit(now) else {
                break;
            };
            server.handle_datagram(now, CLIENT_ADDR, &d).map_err(text)?;
        }
        for _ in 0..256 {
            let Some((_, d)) = server.poll_transmit(now) else {
                break;
            };
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
    let client_leaf = leaf_params("openrelay-android-keystore", false)
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
    let server_cert = leaf_params("openrelay-daemon", true)
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

mod audio;
mod client;
mod enroll;
mod return_audio;

pub(crate) use client::{BridgeEvent, ProductClient, Provision};

#[cfg(test)]
mod udp_roundtrip;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, OnceLock};

use jni::objects::JString;
use jni::sys::{jboolean, jdouble, jint, jlong, jobjectArray, jstring};

/// Handles de producto. Nunca un puntero crudo hacia Java: el id es opaco.
fn sessions() -> &'static Mutex<HashMap<u64, ProductClient>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, ProductClient>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn jbytes(env: &JNIEnv, value: JObject) -> Result<Vec<u8>, String> {
    env.convert_byte_array(JByteArray::from(value))
        .map_err(|e| e.to_string())
}

fn jshorts(env: &JNIEnv, value: &JShortArray) -> Result<Vec<i16>, String> {
    let len = env.get_array_length(value).map_err(text)?;
    let mut out = vec![0i16; len as usize];
    env.get_short_array_region(value, 0, &mut out)
        .map_err(text)?;
    Ok(out)
}

fn jtext(env: &mut JNIEnv, value: &JString) -> Result<String, String> {
    env.get_string(value)
        .map(|s| s.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

fn ret_string(env: &mut JNIEnv, text: String) -> jstring {
    match env.new_string(text) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

fn with_session<T>(
    id: u64,
    f: impl FnOnce(&ProductClient) -> Result<T, String>,
) -> Result<T, String> {
    let guard = sessions()
        .lock()
        .map_err(|_| "lock envenenado".to_string())?;
    let client = guard.get(&id).ok_or("handle desconocido")?;
    f(client)
}

/// Arma la identidad del Keystore y abre el UDP. Devuelve el id, o un error con prefijo `ERR `.
fn open_product(
    env: &mut JNIEnv,
    signer: JObject,
    endpoint: &str,
    server_name: &str,
    ca_der: Vec<u8>,
    pin_hex: &str,
    chain: Vec<Vec<u8>>,
) -> Result<u64, String> {
    let addr: SocketAddr = endpoint.parse().map_err(|_| "endpoint no es host:puerto")?;
    if addr.port() == 0 {
        return Err("puerto de sesión vacío".into());
    }
    let pin = SpkiPin::from_hex(pin_hex).ok_or("pin SPKI no es 64 hex")?;
    if ca_der.is_empty() || chain.is_empty() || chain.len() > identity::MAX_CERT_CHAIN {
        return Err("provisión de certificados vacía o larga".into());
    }
    let vm = env.get_java_vm().map_err(|e| e.to_string())?;
    let signer_ref = env.new_global_ref(&signer).map_err(|e| e.to_string())?;
    let spki_obj = env
        .call_method(&signer, "publicKeyDer", "()[B", &[])
        .map_err(|e| e.to_string())?
        .l()
        .map_err(|e| e.to_string())?;
    let spki = jbytes(env, spki_obj)?;
    let key = Arc::new(KeystoreKey {
        inner: Arc::new(Inner {
            vm,
            signer: signer_ref,
            signatures: AtomicUsize::new(0),
        }),
        spki,
    });
    let chain = chain
        .into_iter()
        .map(CertificateDer::from)
        .collect::<Vec<_>>();
    let client = ProductClient::open(Provision {
        endpoint: addr,
        server_name: server_name.to_string(),
        ca_der: CertificateDer::from(ca_der),
        pin,
        chain,
        key,
    })?;
    let id = next_id();
    sessions()
        .lock()
        .map_err(|_| "lock envenenado".to_string())?
        .insert(id, client);
    Ok(id)
}

fn event_line(event: BridgeEvent) -> String {
    match event {
        BridgeEvent::Authorized => "authorized".into(),
        BridgeEvent::Rejected => "rejected".into(),
        BridgeEvent::Closed => "closed".into(),
        BridgeEvent::IoFault(detail) => format!("io\t{detail}"),
        BridgeEvent::Ack {
            request_id,
            revision,
            applied,
            requested_json,
            effective_json,
        } => {
            format!("ack\t{request_id}\t{revision}\t{applied}\t{requested_json}\t{effective_json}")
        }
        BridgeEvent::Error {
            request_id,
            code,
            current_revision,
        } => match current_revision {
            Some(revision) => format!("error\t{request_id}\t{code}\t{revision}"),
            None => format!("error\t{request_id}\t{code}"),
        },
    }
}

mod exports {
    use super::*;

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_KeystoreHandshakeTest_nativeHandshake(
        mut env: JNIEnv,
        _class: JClass,
        signer: JObject,
    ) -> jstring {
        let report = run(&mut env, signer).unwrap_or_else(|e| format!("ERR {e}"));
        ret_string(&mut env, report)
    }

    /// Abre la sesión de producto. `chain` es `byte[][]` (hoja primero).
    /// Devuelve el handle (>0) o lanza con el texto del error vía el valor negativo codificado en el long alto: 0 y el mensaje por `lastError`.
    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeOpen(
        mut env: JNIEnv,
        _class: JClass,
        signer: JObject,
        endpoint: JString,
        server_name: JString,
        ca_der: JByteArray,
        pin_hex: JString,
        chain: jni::objects::JObjectArray,
    ) -> jlong {
        let opened = (|| {
            let endpoint = jtext(&mut env, &endpoint)?;
            let server_name = jtext(&mut env, &server_name)?;
            let pin_hex = jtext(&mut env, &pin_hex)?;
            let ca = jbytes(&env, JObject::from(ca_der))?;
            let len = env.get_array_length(&chain).map_err(|e| e.to_string())?;
            let mut certs = Vec::with_capacity(len as usize);
            for index in 0..len {
                let item = env
                    .get_object_array_element(&chain, index)
                    .map_err(|e| e.to_string())?;
                certs.push(jbytes(&env, item)?);
            }
            open_product(
                &mut env,
                signer,
                &endpoint,
                &server_name,
                ca,
                &pin_hex,
                certs,
            )
        })();
        match opened {
            Ok(id) => id as jlong,
            Err(error) => {
                remember_error(error);
                0
            }
        }
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeHello(
        _env: JNIEnv,
        _class: JClass,
        handle: jlong,
    ) -> jint {
        call_code(handle as u64, |client| client.hello())
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeConfigGet(
        mut env: JNIEnv,
        _class: JClass,
        handle: jlong,
        key: JString,
    ) -> jstring {
        let key = jtext(&mut env, &key).unwrap_or_default();
        let result = with_session(handle as u64, |client| {
            client.config_get(if key.is_empty() {
                None
            } else {
                Some(key.as_str())
            })
        });
        ret_string(
            &mut env,
            match result {
                Ok(id) => id,
                Err(error) => {
                    remember_error(error.clone());
                    format!("ERR {error}")
                }
            },
        )
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeConfigSetGain(
        mut env: JNIEnv,
        _class: JClass,
        handle: jlong,
        expected_revision: jlong,
        gain_db: jdouble,
    ) -> jstring {
        let result = with_session(handle as u64, |client| {
            client.config_set_gain(expected_revision as u64, gain_db)
        });
        ret_string(
            &mut env,
            match result {
                Ok(id) => id,
                Err(error) => {
                    remember_error(error.clone());
                    format!("ERR {error}")
                }
            },
        )
    }

    /// Fija el perfil de envío: `0` PCM16 (defecto), `1` Opus 5 ms, `2` Opus
    /// 2,5 ms, `3` Opus 10 ms. `0` = aplicado.
    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeSetCodec(
        _env: JNIEnv,
        _class: JClass,
        handle: jlong,
        profile: jint,
    ) -> jint {
        call_code(handle as u64, |client| client.set_codec(profile))
    }

    /// Envía un bloque PCM16 del perfil activo: 240 muestras (5 ms) en PCM16,
    /// o las del perfil Opus. `0` = enviado.
    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeSendAudio(
        env: JNIEnv,
        _class: JClass,
        handle: jlong,
        pcm: JShortArray,
    ) -> jint {
        let samples = match jshorts(&env, &pcm) {
            Ok(samples) => samples,
            Err(error) => {
                remember_error(error);
                return 1;
            }
        };
        call_code(handle as u64, |client| client.send_audio(&samples))
    }

    /// Enciende o apaga el anuncio del retorno `audio.return-pcm16-48k-mono`.
    /// Apagado por defecto: sin esta llamada la feature no viaja.
    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeSetReturnAudio(
        _env: JNIEnv,
        _class: JClass,
        handle: jlong,
        enabled: jboolean,
    ) -> jint {
        call_code(handle as u64, |client| {
            client.set_return_audio(enabled != 0)
        })
    }

    /// Copia un frame de retorno (240 muestras) en `dst`.
    ///
    /// Devuelve las muestras copiadas, `0` si la cola está vacía y negativo si
    /// el handle no existe, el retorno está apagado o `dst` es más corto que
    /// un frame.
    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeReadReturn(
        env: JNIEnv,
        _class: JClass,
        handle: jlong,
        dst: JShortArray,
    ) -> jint {
        let len = match env.get_array_length(&dst) {
            Ok(len) => len as usize,
            Err(error) => {
                remember_error(error.to_string());
                return -1;
            }
        };
        if len < openrelay_audio_core::SAMPLE_COUNT {
            remember_error("dst de retorno más corto que un frame".into());
            return -1;
        }
        let mut buf = vec![0i16; len];
        let read = match with_session(handle as u64, |client| client.read_return(&mut buf)) {
            Ok(read) => read,
            Err(error) => {
                remember_error(error);
                return -1;
            }
        };
        if read == 0 {
            return 0;
        }
        if let Err(error) = env.set_short_array_region(&dst, 0, &buf[..read]) {
            remember_error(error.to_string());
            return -1;
        }
        read as jint
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativePoll(
        mut env: JNIEnv,
        _class: JClass,
        handle: jlong,
    ) -> jstring {
        let result = with_session(handle as u64, |client| client.poll_event());
        ret_string(
            &mut env,
            match result {
                Ok(Some(event)) => event_line(event),
                Ok(None) => String::new(),
                Err(error) => format!("ERR {error}"),
            },
        )
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeWaitAuthorized(
        _env: JNIEnv,
        _class: JClass,
        handle: jlong,
    ) -> jint {
        call_code(handle as u64, |client| client.wait_authorized())
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeWaitControl(
        mut env: JNIEnv,
        _class: JClass,
        handle: jlong,
        request_id: JString,
    ) -> jstring {
        let request_id = jtext(&mut env, &request_id).unwrap_or_default();
        let result = with_session(handle as u64, |client| client.wait_control(&request_id));
        ret_string(
            &mut env,
            match result {
                Ok(event) => event_line(event),
                Err(error) => format!("ERR {error}"),
            },
        )
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeClose(
        _env: JNIEnv,
        _class: JClass,
        handle: jlong,
    ) -> jint {
        let id = handle as u64;
        let mut guard = match sessions().lock() {
            Ok(guard) => guard,
            Err(_) => return 1,
        };
        match guard.remove(&id) {
            Some(mut client) => match client.close() {
                Ok(()) => 0,
                Err(_) => 1,
            },
            None => 0,
        }
    }

    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_SessionClient_nativeLastError(
        mut env: JNIEnv,
        _class: JClass,
    ) -> jstring {
        let text = LAST_ERROR
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_default();
        ret_string(&mut env, text)
    }

    /// Emparejamiento desde el QR: `byte[][]` de 3 elementos
    /// `[hoja DER, CA de sesión DER, puerto BE16]`, o null y el motivo por
    /// `nativeLastError`. Bloquea mientras el usuario aprueba en el PC.
    #[allow(unsafe_code)]
    #[no_mangle]
    pub extern "system" fn Java_dev_openrelay_app_PairingFlow_nativeEnroll(
        mut env: JNIEnv,
        _class: JClass,
        host: JString,
        port: jint,
        pin_hex: JString,
        token_hex: JString,
        spki: JByteArray,
    ) -> jobjectArray {
        let result = (|| {
            let host = jtext(&mut env, &host)?;
            let pin_hex = jtext(&mut env, &pin_hex)?;
            let token_hex = jtext(&mut env, &token_hex)?;
            let spki = jbytes(&env, JObject::from(spki))?;
            if !(1..=65535).contains(&port) {
                return Err("puerto de enrolamiento fuera de rango".to_string());
            }
            crate::enroll::enroll(&host, port as u16, &pin_hex, &token_hex, spki)
        })();
        match result {
            Ok(enrolled) => match enrolled_array(&mut env, enrolled) {
                Ok(array) => array,
                Err(error) => {
                    remember_error(error);
                    std::ptr::null_mut()
                }
            },
            Err(error) => {
                remember_error(error);
                std::ptr::null_mut()
            }
        }
    }
}

/// `byte[][]` con hoja, CA y puerto BE16. El orden lo fija `Pairing.kt`.
fn enrolled_array(
    env: &mut JNIEnv,
    enrolled: crate::enroll::Enrolled,
) -> Result<jobjectArray, String> {
    let leaf = env
        .byte_array_from_slice(&enrolled.leaf_der)
        .map_err(text)?;
    let ca = env.byte_array_from_slice(&enrolled.ca_der).map_err(text)?;
    let port = env
        .byte_array_from_slice(&enrolled.port.to_be_bytes())
        .map_err(text)?;
    let array = env
        .new_object_array(3, "[B", JObject::null())
        .map_err(text)?;
    env.set_object_array_element(&array, 0, leaf)
        .map_err(text)?;
    env.set_object_array_element(&array, 1, ca)
        .map_err(text)?;
    env.set_object_array_element(&array, 2, port)
        .map_err(text)?;
    Ok(array.into_raw())
}

fn call_code(id: u64, f: impl FnOnce(&ProductClient) -> Result<(), String>) -> jint {
    match with_session(id, f) {
        Ok(()) => 0,
        Err(error) => {
            remember_error(error);
            1
        }
    }
}

fn remember_error(error: String) {
    if let Ok(mut guard) = LAST_ERROR.lock() {
        *guard = Some(error);
    }
}

static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
