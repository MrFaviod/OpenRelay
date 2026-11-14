//! Contrato de la máquina de control y del wire de control `arl.control/v1`.
//!
//! Cada prueba usa el camino real: JSON de entrada → framing de 4 bytes BE →
//! parser estricto → mensaje tipado → máquina de control → respuesta codificada.
//! El audio no se implementa aquí: sólo se citan sus límites del contrato.

use arl_proto::control::{
    classify_remote_action, config_keys, AuthorizedPeer, Capabilities, ConfigKey, ConfigPair,
    ConfigState, ConfigValue, ControlSession, EffectivePolicy, ErrorCode, ErrorMsg, Feature,
    IdentityPolicy, Outcome, PeerRole, RemoteAction, SessionState, ValueType,
};
use arl_proto::wire::{
    self, AckEffective, AckMsg, CapabilitiesMsg, ConfigGetMsg, ConfigSetMsg, ControlMessage,
    EventBody, EventMsg, HelloMsg, MAX_REVISION,
};

const NOW: u64 = 1_000;

fn peer() -> AuthorizedPeer {
    AuthorizedPeer::new(
        "sess-0001",
        PeerRole::Android,
        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
    )
}

fn session() -> ControlSession {
    ControlSession::new(peer(), ConfigState::new(), Box::new(IdentityPolicy))
}

fn key(name: &str) -> ConfigKey {
    ConfigKey::new(name).expect("clave válida")
}

fn frame(json: &str) -> Vec<u8> {
    wire::encode_frame(json.as_bytes()).expect("frame válido")
}

fn send(session: &mut ControlSession, json: &str, now: u64) -> Outcome {
    session.handle_frame(&frame(json), now)
}

fn set_json(request_id: &str, revision: u64, k: &str, value: &str) -> String {
    format!(
        r#"{{"schema_version":"arl.control/v1","type":"config.set","request_id":"{request_id}","expected_revision":{revision},"payload":{{"key":"{k}","value":{value}}}}}"#
    )
}

fn get_json(request_id: &str, k: Option<&str>) -> String {
    match k {
        Some(name) => format!(
            r#"{{"schema_version":"arl.control/v1","type":"config.get","request_id":"{request_id}","payload":{{"key":"{name}"}}}}"#
        ),
        None => format!(
            r#"{{"schema_version":"arl.control/v1","type":"config.get","request_id":"{request_id}","payload":{{"key":null}}}}"#
        ),
    }
}

fn expect_ack(outcome: &Outcome) -> &AckMsg {
    match outcome.response.as_ref() {
        Some(ControlMessage::Ack(ack)) => ack,
        other => panic!("se esperaba ack, llegó {other:?}"),
    }
}

fn expect_error(outcome: &Outcome) -> &ErrorMsg {
    match outcome.response.as_ref() {
        Some(ControlMessage::Error(error)) => error,
        other => panic!("se esperaba error, llegó {other:?}"),
    }
}

fn error_code(outcome: &Outcome) -> ErrorCode {
    expect_error(outcome).code
}

// ---------------------------------------------------------------- framing --

#[test]
fn frame_length_prefix_is_big_endian_and_bounded() {
    assert_eq!(wire::decode_frame(&[0, 0, 0, 1, b'x']).unwrap(), b"x");
    assert_eq!(
        wire::decode_frame(&[0, 0, 0, 0]).unwrap_err().code(),
        ErrorCode::Invalid
    );
    assert_eq!(
        wire::decode_frame(&[0, 0, 0x40, 0x01]).unwrap_err().code(),
        ErrorCode::TooLarge
    );
    assert_eq!(
        wire::decode_frame(&[0, 0, 0, 5, b'{']).unwrap_err().code(),
        ErrorCode::Invalid
    );
    // Exactamente un frame: bytes de más tras `length` son framing corrupto.
    assert_eq!(
        wire::decode_frame(&[0, 0, 0, 1, b'x', b'y'])
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    assert_eq!(
        wire::encode_frame(&[]).unwrap_err().code(),
        ErrorCode::Invalid
    );
    let max = vec![b' '; wire::MAX_CONTROL_PAYLOAD_BYTES];
    let framed = wire::encode_frame(&max).unwrap();
    assert_eq!(framed.len(), 4 + wire::MAX_CONTROL_PAYLOAD_BYTES);
    assert_eq!(wire::decode_frame(&framed).unwrap().len(), max.len());
    let too_large = vec![b' '; wire::MAX_CONTROL_PAYLOAD_BYTES + 1];
    assert_eq!(
        wire::encode_frame(&too_large).unwrap_err().code(),
        ErrorCode::TooLarge
    );
}

#[test]
fn oversized_frame_closes_and_reports_too_large() {
    let mut session = session();
    let payload = vec![b' '; wire::MAX_CONTROL_PAYLOAD_BYTES + 1];
    let mut raw = (payload.len() as u32).to_be_bytes().to_vec();
    raw.extend_from_slice(&payload);
    let outcome = session.handle_frame(&raw, NOW);
    assert!(outcome.close);
    assert_eq!(error_code(&outcome), ErrorCode::TooLarge);
}

// -------------------------------------------------------------------- JSON --

#[test]
fn json_rejects_duplicate_keys() {
    let duplicate = br#"{"schema_version":"arl.control/v1","type":"ping","request_id":"a","payload":{},"payload":{}}"#;
    assert_eq!(
        wire::parse_json(duplicate).unwrap_err().code(),
        ErrorCode::Invalid
    );
    let nested = br#"{"a":{"b":1,"b":2}}"#;
    assert_eq!(
        wire::parse_json(nested).unwrap_err().code(),
        ErrorCode::Invalid
    );
}

#[test]
fn json_rejects_bom_trailing_and_nonstandard_numbers() {
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(br#"{"a":1}"#);
    assert_eq!(
        wire::parse_json(&bom).unwrap_err().code(),
        ErrorCode::Invalid
    );
    assert_eq!(
        wire::parse_json(br#"{"a":1} x"#).unwrap_err().code(),
        ErrorCode::Invalid
    );
    for bad in [r#"{"a":NaN}"#, r#"{"a":Infinity}"#, r#"{"a":-Infinity}"#] {
        assert_eq!(
            wire::parse_json(bad.as_bytes()).unwrap_err().code(),
            ErrorCode::Invalid
        );
    }
}

#[test]
fn json_enforces_depth_key_array_and_string_limits() {
    assert!(wire::parse_json(&nested_arrays(8).into_bytes()).is_ok());
    assert_eq!(
        wire::parse_json(&nested_arrays(9).into_bytes())
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );

    assert!(wire::parse_json(&object_with_keys(64).into_bytes()).is_ok());
    assert_eq!(
        wire::parse_json(&object_with_keys(65).into_bytes())
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );

    assert!(wire::parse_json(&array_with_items(64).into_bytes()).is_ok());
    assert_eq!(
        wire::parse_json(&array_with_items(65).into_bytes())
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );

    let s1024 = format!(r#"{{"s":"{}"}}"#, "é".repeat(1024));
    assert!(wire::parse_json(s1024.as_bytes()).is_ok());
    let s1025 = format!(r#"{{"s":"{}"}}"#, "é".repeat(1025));
    assert_eq!(
        wire::parse_json(s1025.as_bytes()).unwrap_err().code(),
        ErrorCode::Invalid
    );
}

fn nested_arrays(depth: usize) -> String {
    format!("{}0{}", "[".repeat(depth), "]".repeat(depth))
}

fn object_with_keys(count: usize) -> String {
    let body: Vec<String> = (0..count).map(|i| format!("\"k{i}\":{i}")).collect();
    format!("{{{}}}", body.join(","))
}

fn array_with_items(count: usize) -> String {
    let body: Vec<String> = (0..count).map(|i| i.to_string()).collect();
    format!("[{}]", body.join(","))
}

// ----------------------------------------------------------------- envelope --

#[test]
fn envelope_requires_request_id_and_rejects_unknown_fields() {
    let missing =
        br#"{"schema_version":"arl.control/v1","type":"config.get","payload":{"key":null}}"#;
    assert_eq!(
        wire::decode_message(&frame_len(missing))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    let unknown = br#"{"schema_version":"arl.control/v1","type":"ping","request_id":"p1","payload":{},"extra":1}"#;
    assert_eq!(
        wire::decode_message(&frame_len(unknown))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    let missing_payload = br#"{"schema_version":"arl.control/v1","type":"ping","request_id":"p1"}"#;
    assert_eq!(
        wire::decode_message(&frame_len(missing_payload))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
}

fn frame_len(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

#[test]
fn envelope_restricts_revision_fields_per_type() {
    for bad in [
        r#"{"schema_version":"arl.control/v1","type":"config.get","request_id":"g1","revision":1,"payload":{"key":null}}"#,
        r#"{"schema_version":"arl.control/v1","type":"config.set","request_id":"s1","expected_revision":0,"revision":1,"payload":{"key":"audio.mute","value":true}}"#,
        r#"{"schema_version":"arl.control/v1","type":"ping","request_id":"p1","expected_revision":0,"payload":{}}"#,
        r#"{"schema_version":"arl.control/v1","type":"ping","request_id":"p1","payload":{"x":1}}"#,
        r#"{"schema_version":"arl.control/v1","type":"config.set","request_id":"s1","payload":{"key":"audio.mute","value":true}}"#,
        r#"{"schema_version":"arl.control/v1","type":"config.get","request_id":"g1","payload":{"key":null,"x":1}}"#,
    ] {
        assert_eq!(
            wire::decode_message(&frame_len(bad.as_bytes()))
                .unwrap_err()
                .code(),
            ErrorCode::Invalid,
            "{bad}"
        );
    }
}

#[test]
fn request_id_and_revision_ranges_are_exact() {
    for rid in ["a", "A-z_09", &"a".repeat(64)] {
        let json = format!(
            r#"{{"schema_version":"arl.control/v1","type":"ping","request_id":"{rid}","payload":{{}}}}"#
        );
        assert!(wire::decode_message(&frame_len(json.as_bytes())).is_ok());
    }
    for rid in ["", "bad id", &"a".repeat(65), "ñ"] {
        let json = format!(
            r#"{{"schema_version":"arl.control/v1","type":"ping","request_id":"{rid}","payload":{{}}}}"#
        );
        assert_eq!(
            wire::decode_message(&frame_len(json.as_bytes()))
                .unwrap_err()
                .code(),
            ErrorCode::Invalid
        );
    }
    let max_ok = format!(
        r#"{{"schema_version":"arl.control/v1","type":"ack","request_id":"a1","revision":{MAX_REVISION},"payload":{{"applied":false,"requested":null,"effective":{{}}}}}}"#
    );
    assert!(wire::decode_message(&frame_len(max_ok.as_bytes())).is_ok());
    for bad_revision in ["9007199254740992", "1.5", "-1"] {
        let json = format!(
            r#"{{"schema_version":"arl.control/v1","type":"ack","request_id":"a1","revision":{bad_revision},"payload":{{"applied":false,"requested":null,"effective":{{}}}}}}"#
        );
        assert_eq!(
            wire::decode_message(&frame_len(json.as_bytes()))
                .unwrap_err()
                .code(),
            ErrorCode::Invalid,
            "revision {bad_revision}"
        );
    }
}

#[test]
fn encode_decode_roundtrip_for_all_control_types() {
    let messages = vec![
        ControlMessage::Hello(HelloMsg {
            request_id: "h-1".into(),
            role: PeerRole::Android,
            version: "0.1.0".into(),
            protocol_version: "arl.control/v1".into(),
        }),
        ControlMessage::Capabilities(CapabilitiesMsg {
            request_id: "c-1".into(),
            revision: 4,
            capabilities: Capabilities::from_peer(
                "device-0001",
                PeerRole::LinuxDaemon,
                vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
            ),
        }),
        ControlMessage::ConfigGet(ConfigGetMsg {
            request_id: "g-1".into(),
            key: Some(key("audio.gain_db")),
        }),
        ControlMessage::ConfigGet(ConfigGetMsg {
            request_id: "g-2".into(),
            key: None,
        }),
        ControlMessage::ConfigSet(ConfigSetMsg {
            request_id: "s-1".into(),
            expected_revision: 7,
            key: key("audio.gain_db"),
            value: ConfigValue::Number(3.0),
        }),
        ControlMessage::Ack(AckMsg {
            request_id: "s-1".into(),
            revision: 8,
            applied: true,
            requested: Some(ConfigPair {
                key: key("audio.gain_db"),
                value: ConfigValue::Number(3.0),
            }),
            effective: AckEffective::Pair(ConfigPair {
                key: key("audio.gain_db"),
                value: ConfigValue::Number(3.0),
            }),
        }),
        ControlMessage::Ack(AckMsg {
            request_id: "g-2".into(),
            revision: 8,
            applied: true,
            requested: None,
            effective: AckEffective::Snapshot(ConfigState::new().snapshot()),
        }),
        ControlMessage::Error(
            ErrorMsg::new("s-2", ErrorCode::Conflict, "revision mismatch")
                .unwrap()
                .with_current_revision(9)
                .unwrap(),
        ),
        ControlMessage::Error(ErrorMsg::new("s-3", ErrorCode::LocalKey, "key is local").unwrap()),
        ControlMessage::Event(EventMsg {
            revision: 8,
            event: EventBody::ConfigChanged {
                key: key("audio.gain_db"),
                requested: ConfigValue::Number(3.0),
                effective: ConfigValue::Number(3.0),
            },
        }),
        ControlMessage::Event(EventMsg {
            revision: 8,
            event: EventBody::SessionState {
                state: SessionState::Closed,
                detail: Some("client closed".into()),
            },
        }),
        ControlMessage::Ping {
            request_id: "p-1".into(),
        },
        ControlMessage::Pong {
            request_id: "p-1".into(),
        },
    ];
    for message in messages {
        let encoded = wire::encode_message(&message).expect("encode");
        let decoded = wire::decode_message(&encoded).expect("decode");
        assert_eq!(decoded, message);
    }
}

#[test]
fn encoded_messages_validate_against_contract_schema() {
    let schema = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/control-v1.schema.json"
    );
    let python_ok = std::process::Command::new("python3")
        .args(["-c", "import jsonschema"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !python_ok {
        eprintln!("python3+jsonschema no disponible: fixture externo omitido");
        return;
    }
    let messages = vec![
        ControlMessage::Hello(HelloMsg {
            request_id: "h-1".into(),
            role: PeerRole::Android,
            version: "0.1.0".into(),
            protocol_version: "arl.control/v1".into(),
        }),
        ControlMessage::Capabilities(CapabilitiesMsg {
            request_id: "c-1".into(),
            revision: 0,
            capabilities: Capabilities::from_peer(
                "device-0001",
                PeerRole::LinuxDaemon,
                vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
            ),
        }),
        ControlMessage::ConfigGet(ConfigGetMsg {
            request_id: "g-1".into(),
            key: None,
        }),
        ControlMessage::ConfigSet(ConfigSetMsg {
            request_id: "s-1".into(),
            expected_revision: 0,
            key: key("audio.gain_db"),
            value: ConfigValue::Number(24.0),
        }),
        ControlMessage::Ack(AckMsg {
            request_id: "s-1".into(),
            revision: 1,
            applied: true,
            requested: Some(ConfigPair {
                key: key("audio.gain_db"),
                value: ConfigValue::Number(-24.0),
            }),
            effective: AckEffective::Snapshot(ConfigState::new().snapshot()),
        }),
        ControlMessage::Error(
            ErrorMsg::new("s-2", ErrorCode::Conflict, "revision mismatch")
                .unwrap()
                .with_current_revision(1)
                .unwrap(),
        ),
        ControlMessage::Event(EventMsg {
            revision: 1,
            event: EventBody::ConfigChanged {
                key: key("audio.gain_db"),
                requested: ConfigValue::Number(3.0),
                effective: ConfigValue::Number(3.0),
            },
        }),
        ControlMessage::Event(EventMsg {
            revision: 1,
            event: EventBody::SessionState {
                state: SessionState::Active,
                detail: None,
            },
        }),
        ControlMessage::Ping {
            request_id: "p-1".into(),
        },
        ControlMessage::Pong {
            request_id: "p-1".into(),
        },
    ];
    let payloads: Vec<String> = messages
        .iter()
        .map(|message| {
            let encoded = wire::encode_message(message).expect("encode");
            String::from_utf8(encoded[4..].to_vec()).expect("utf-8")
        })
        .collect();
    let document = format!("[{}]", payloads.join(","));
    let dir = std::env::temp_dir();
    let fixture = dir.join(format!("arl-proto-fixture-{}.json", std::process::id()));
    std::fs::write(&fixture, document).expect("fixture");
    let script = r#"
import json, sys
import jsonschema
schema = json.load(open(sys.argv[1], encoding="utf-8"))
docs = json.load(open(sys.argv[2], encoding="utf-8"))
validator = jsonschema.Draft202012Validator(schema)
for index, doc in enumerate(docs):
    errors = sorted(validator.iter_errors(doc), key=lambda e: list(e.path))
    if errors:
        raise SystemExit(f"doc {index}: {errors[0].message}")
print(f"ok {len(docs)}")
"#;
    let output = std::process::Command::new("python3")
        .args(["-c", script, schema])
        .arg(&fixture)
        .output()
        .expect("python3");
    let _ = std::fs::remove_file(&fixture);
    assert!(
        output.status.success(),
        "schema inválido: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok 10");
}

// ------------------------------------------------------------ config state --

#[test]
fn set_applies_increments_revision_and_emits_closed_event() {
    let mut session = session();
    let outcome = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    let ack = expect_ack(&outcome);
    assert!(ack.applied);
    assert_eq!(ack.revision, 1);
    assert_eq!(
        ack.requested.as_ref().unwrap().value,
        ConfigValue::Number(3.0)
    );
    assert_eq!(
        ack.effective,
        AckEffective::Pair(ConfigPair {
            key: key("audio.gain_db"),
            value: ConfigValue::Number(3.0),
        })
    );
    assert_eq!(session.revision(), 1);
    assert_eq!(outcome.events.len(), 1);
    match &outcome.events[0] {
        ControlMessage::Event(EventMsg {
            revision: 1,
            event:
                EventBody::ConfigChanged {
                    key: event_key,
                    requested,
                    effective,
                },
        }) => {
            assert_eq!(event_key.as_str(), "audio.gain_db");
            assert_eq!(*requested, ConfigValue::Number(3.0));
            assert_eq!(*effective, ConfigValue::Number(3.0));
        }
        other => panic!("evento inesperado: {other:?}"),
    }
}

#[test]
fn set_same_effective_value_is_applied_false_without_event() {
    let mut session = session();
    let outcome = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "0.0"),
        NOW,
    );
    let ack = expect_ack(&outcome);
    assert!(!ack.applied);
    assert_eq!(ack.revision, 0);
    assert_eq!(session.revision(), 0);
    assert!(outcome.events.is_empty());
}

#[test]
fn retry_same_request_id_replays_without_reapplying() {
    let mut session = session();
    let raw = set_json("c-1", 0, "audio.gain_db", "3.0");
    let first = send(&mut session, &raw, NOW);
    let second = send(&mut session, &raw, NOW + 1);
    assert_eq!(first.response, second.response);
    assert!(second.events.is_empty());
    assert_eq!(session.revision(), 1);
}

#[test]
fn idempotent_replay_returns_original_revision_after_other_changes() {
    let mut session = session();
    let raw = set_json("c-1", 0, "audio.gain_db", "3.0");
    let first = send(&mut session, &raw, NOW);
    let _ = send(
        &mut session,
        &set_json("c-2", 1, "audio.mute", "true"),
        NOW + 1,
    );
    assert_eq!(session.revision(), 2);
    let replay = send(&mut session, &raw, NOW + 2);
    assert_eq!(first.response, replay.response);
    assert_eq!(expect_ack(&replay).revision, 1);
    assert!(replay.events.is_empty());
}

#[test]
fn same_request_id_with_other_payload_is_request_id_reuse() {
    let mut session = session();
    let _ = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    let outcome = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "4.0"),
        NOW + 1,
    );
    assert_eq!(error_code(&outcome), ErrorCode::RequestIdReuse);
    assert!(!expect_error(&outcome).code.retryable());
    assert_eq!(session.revision(), 1);
}

#[test]
fn request_identity_is_independent_of_json_key_order() {
    let mut session = session();
    let first = r#"{"schema_version":"arl.control/v1","type":"config.set","request_id":"c-9","expected_revision":0,"payload":{"key":"audio.mute","value":true}}"#;
    let reordered = r#"{"payload":{"value":true,"key":"audio.mute"},"expected_revision":0,"request_id":"c-9","type":"config.set","schema_version":"arl.control/v1"}"#;
    let a = send(&mut session, first, NOW);
    let b = send(&mut session, reordered, NOW + 1);
    assert_eq!(a.response, b.response);
    assert!(b.events.is_empty());
    assert_eq!(session.revision(), 1);
}

#[test]
fn conflict_reports_current_revision_and_applies_nothing() {
    let mut session = session();
    let _ = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    let outcome = send(
        &mut session,
        &set_json("c-2", 0, "audio.gain_db", "4.0"),
        NOW + 1,
    );
    let error = expect_error(&outcome);
    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(error.code.retryable());
    assert_eq!(error.current_revision, Some(1));
    assert!(outcome.events.is_empty());
    assert_eq!(session.revision(), 1);
    assert_eq!(
        session.snapshot().get(&key("audio.gain_db")),
        Some(&ConfigValue::Number(3.0))
    );
}

#[test]
fn retry_of_conflict_after_reading_revision_uses_new_request_id() {
    let mut session = session();
    let _ = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    let conflict = send(
        &mut session,
        &set_json("c-2", 0, "audio.gain_db", "4.0"),
        NOW + 1,
    );
    assert_eq!(error_code(&conflict), ErrorCode::Conflict);
    // Reintentar con el mismo id y otra revisión esperada cambia la identidad
    // canónica de la operación: request_id_reuse, nunca una aplicación.
    let reused = send(
        &mut session,
        &set_json("c-2", 1, "audio.gain_db", "4.0"),
        NOW + 2,
    );
    assert_eq!(error_code(&reused), ErrorCode::RequestIdReuse);
    assert!(!expect_error(&reused).code.retryable());
    assert_eq!(session.revision(), 1);
    // El conflicto original queda cacheado y se reproduce tal cual, sin
    // reevaluarse contra la revisión vigente.
    let replay = send(
        &mut session,
        &set_json("c-2", 0, "audio.gain_db", "4.0"),
        NOW + 3,
    );
    assert_eq!(replay.response, conflict.response);
    assert!(replay.events.is_empty());
    assert_eq!(session.revision(), 1);
    // El reintento correcto usa un request_id nuevo.
    let retry = send(
        &mut session,
        &set_json("c-3", 1, "audio.gain_db", "4.0"),
        NOW + 4,
    );
    assert!(expect_ack(&retry).applied);
    assert_eq!(session.revision(), 2);
    assert_eq!(retry.events.len(), 1);
    let replay_retry = send(
        &mut session,
        &set_json("c-3", 1, "audio.gain_db", "4.0"),
        NOW + 5,
    );
    assert_eq!(replay_retry.response, retry.response);
    assert!(replay_retry.events.is_empty());
    assert_eq!(session.revision(), 2);
}

#[test]
fn cached_conflict_replays_original_after_global_revision_advances() {
    let mut session = session();
    let _ = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    let conflict = send(
        &mut session,
        &set_json("c-2", 0, "audio.mute", "true"),
        NOW + 1,
    );
    assert_eq!(error_code(&conflict), ErrorCode::Conflict);
    assert_eq!(expect_error(&conflict).current_revision, Some(1));
    let applied = send(
        &mut session,
        &set_json("c-3", 1, "audio.mute", "true"),
        NOW + 2,
    );
    assert!(expect_ack(&applied).applied);
    assert_eq!(session.revision(), 2);
    // La revisión avanzó, pero el reintento idéntico devuelve el conflicto
    // original guardado (current_revision de entonces), no uno nuevo.
    let replay = send(
        &mut session,
        &set_json("c-2", 0, "audio.mute", "true"),
        NOW + 3,
    );
    assert_eq!(replay.response, conflict.response);
    assert_eq!(expect_error(&replay).current_revision, Some(1));
    assert!(replay.events.is_empty());
    assert_eq!(session.revision(), 2);
    assert_eq!(
        session.snapshot().get(&key("audio.mute")),
        Some(&ConfigValue::Bool(true))
    );
}

#[test]
fn same_request_id_with_other_expected_revision_is_request_id_reuse() {
    let mut session = session();
    let first = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    assert!(expect_ack(&first).applied);
    let reused = send(
        &mut session,
        &set_json("c-1", 1, "audio.gain_db", "3.0"),
        NOW + 1,
    );
    assert_eq!(error_code(&reused), ErrorCode::RequestIdReuse);
    assert_eq!(session.revision(), 1);
    assert_eq!(
        session.snapshot().get(&key("audio.gain_db")),
        Some(&ConfigValue::Number(3.0))
    );
    let replay = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW + 2,
    );
    assert_eq!(replay.response, first.response);
    assert_eq!(session.revision(), 1);
}

#[test]
fn equivalent_numeric_representation_is_the_same_operation() {
    let mut session = session();
    let first = send(&mut session, &set_json("n-1", 0, "audio.gain_db", "3"), NOW);
    assert!(expect_ack(&first).applied);
    // JSON no distingue 3 de 3.0: misma operación canónica, replay sin aplicar.
    let replay = send(
        &mut session,
        &set_json("n-1", 0, "audio.gain_db", "3.0"),
        NOW + 1,
    );
    assert_eq!(replay.response, first.response);
    assert!(replay.events.is_empty());
    assert_eq!(session.revision(), 1);
    // Un número distinto con el mismo id no colisiona con el anterior.
    let reused = send(
        &mut session,
        &set_json("n-1", 0, "audio.gain_db", "3.5"),
        NOW + 2,
    );
    assert_eq!(error_code(&reused), ErrorCode::RequestIdReuse);
    assert_eq!(session.revision(), 1);
}

#[test]
fn local_and_readonly_keys_are_rejected_without_changes() {
    let mut session = session();
    let local = send(
        &mut session,
        &set_json("c-1", 0, "ui.theme", "\"dark\""),
        NOW,
    );
    assert_eq!(error_code(&local), ErrorCode::LocalKey);
    let readonly = send(
        &mut session,
        &set_json("c-2", 0, "device.model", "\"x\""),
        NOW + 1,
    );
    assert_eq!(error_code(&readonly), ErrorCode::Denied);
    assert_eq!(session.revision(), 0);
    assert!(session
        .handle(
            &ControlMessage::ConfigGet(ConfigGetMsg {
                request_id: "g-1".into(),
                key: Some(key("ui.theme")),
            }),
            NOW
        )
        .response
        .is_some());
}

#[test]
fn unknown_key_is_invalid_on_set_and_not_found_on_get() {
    let mut session = session();
    let set = send(&mut session, &set_json("c-1", 0, "audio.nope", "1"), NOW);
    assert_eq!(error_code(&set), ErrorCode::Invalid);
    let get = send(&mut session, &get_json("g-1", Some("audio.nope")), NOW + 1);
    assert_eq!(error_code(&get), ErrorCode::NotFound);
    assert_eq!(session.revision(), 0);
}

#[test]
fn get_returns_pair_and_snapshot_with_applied_true_and_null_requested() {
    let mut session = session();
    let pair = send(
        &mut session,
        &get_json("g-1", Some("audio.jitter_target_ms")),
        NOW,
    );
    let ack = expect_ack(&pair);
    assert!(ack.applied);
    assert_eq!(ack.revision, 0);
    assert_eq!(ack.requested, None);
    assert_eq!(
        ack.effective,
        AckEffective::Pair(ConfigPair {
            key: key("audio.jitter_target_ms"),
            value: ConfigValue::Integer(10),
        })
    );
    let snapshot = send(&mut session, &get_json("g-2", None), NOW + 1);
    let ack = expect_ack(&snapshot);
    assert_eq!(ack.revision, 0);
    assert!(ack.requested.is_none());
    match &ack.effective {
        AckEffective::Snapshot(snapshot) => {
            assert_eq!(snapshot.len(), config_keys().len());
            assert_eq!(
                snapshot.get(&key("audio.sample_rate_hz")),
                Some(&ConfigValue::Integer(48000))
            );
            assert_eq!(
                snapshot.get(&key("audio.frame_duration_ms")),
                Some(&ConfigValue::Integer(5))
            );
            assert_eq!(
                snapshot.get(&key("device.model")),
                Some(&ConfigValue::String("unknown".into()))
            );
        }
        other => panic!("se esperaba snapshot: {other:?}"),
    }
    assert_eq!(session.revision(), 0);
}

#[test]
fn precedence_shape_before_conflict_and_conflict_before_scope() {
    let mut session = session();
    let _ = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "3.0"),
        NOW,
    );
    // Forma inválida (0.3 no es múltiplo de 0.5) + revisión vieja => invalid.
    let shape = send(
        &mut session,
        &set_json("c-2", 0, "audio.gain_db", "0.3"),
        NOW + 1,
    );
    assert_eq!(error_code(&shape), ErrorCode::Invalid);
    // Clave local + revisión vieja => conflict (conflicto antes que alcance).
    let scope = send(
        &mut session,
        &set_json("c-3", 0, "ui.theme", "\"dark\""),
        NOW + 2,
    );
    assert_eq!(error_code(&scope), ErrorCode::Conflict);
    // Clave desconocida + revisión vieja => conflict (antes que not_found).
    let unknown = send(
        &mut session,
        &set_json("c-4", 0, "audio.nope", "1"),
        NOW + 3,
    );
    assert_eq!(error_code(&unknown), ErrorCode::Conflict);
}

#[test]
fn constants_cannot_be_renegotiated() {
    let mut session = session();
    let bad = send(
        &mut session,
        &set_json("c-1", 0, "audio.sample_rate_hz", "44100"),
        NOW,
    );
    assert_eq!(error_code(&bad), ErrorCode::Invalid);
    let same = send(
        &mut session,
        &set_json("c-2", 0, "audio.sample_rate_hz", "48000"),
        NOW + 1,
    );
    assert!(!expect_ack(&same).applied);
    assert_eq!(session.revision(), 0);
}

// -------------------------------------------------------- effective policy --

struct ClampGain;

impl EffectivePolicy for ClampGain {
    fn effective(&self, key: &ConfigKey, requested: &ConfigValue) -> ConfigValue {
        if key.as_str() == "audio.gain_db" {
            if let ConfigValue::Number(value) = requested {
                return ConfigValue::Number(value.clamp(-6.0, 6.0));
            }
        }
        requested.clone()
    }
}

struct InvalidPolicy;

impl EffectivePolicy for InvalidPolicy {
    fn effective(&self, _key: &ConfigKey, _requested: &ConfigValue) -> ConfigValue {
        ConfigValue::Integer(44100)
    }
}

#[test]
fn effective_can_differ_from_requested_and_is_revalidated() {
    let mut session = ControlSession::new(peer(), ConfigState::new(), Box::new(ClampGain));
    let outcome = send(
        &mut session,
        &set_json("c-1", 0, "audio.gain_db", "24.0"),
        NOW,
    );
    let ack = expect_ack(&outcome);
    assert!(ack.applied);
    assert_eq!(
        ack.requested.as_ref().unwrap().value,
        ConfigValue::Number(24.0)
    );
    assert_eq!(
        ack.effective,
        AckEffective::Pair(ConfigPair {
            key: key("audio.gain_db"),
            value: ConfigValue::Number(6.0),
        })
    );
    match &outcome.events[0] {
        ControlMessage::Event(EventMsg {
            event:
                EventBody::ConfigChanged {
                    requested,
                    effective,
                    ..
                },
            ..
        }) => {
            assert_eq!(*requested, ConfigValue::Number(24.0));
            assert_eq!(*effective, ConfigValue::Number(6.0));
        }
        other => panic!("evento inesperado: {other:?}"),
    }
    // El efectivo ya era 6.0: applied:false sin cambio de revisión.
    let again = send(
        &mut session,
        &set_json("c-2", 1, "audio.gain_db", "24.0"),
        NOW + 1,
    );
    assert!(!expect_ack(&again).applied);
    assert_eq!(session.revision(), 1);

    let mut invalid = ControlSession::new(peer(), ConfigState::new(), Box::new(InvalidPolicy));
    let outcome = send(
        &mut invalid,
        &set_json("c-3", 0, "audio.sample_rate_hz", "48000"),
        NOW,
    );
    assert_eq!(error_code(&outcome), ErrorCode::Internal);
    let outcome = send(
        &mut invalid,
        &set_json("c-4", 0, "audio.gain_db", "3.0"),
        NOW + 1,
    );
    assert_eq!(error_code(&outcome), ErrorCode::Internal);
    assert_eq!(invalid.revision(), 0);
}

struct ClampAudioNova;

impl EffectivePolicy for ClampAudioNova {
    fn effective(&self, key: &ConfigKey, requested: &ConfigValue) -> ConfigValue {
        if key.as_str() == "audio.sample_rate_hz" {
            return ConfigValue::Integer(44100);
        }
        requested.clone()
    }
}

#[test]
fn policy_cannot_override_fixed_format() {
    let mut session = ControlSession::new(peer(), ConfigState::new(), Box::new(ClampAudioNova));
    let outcome = send(
        &mut session,
        &set_json("c-1", 0, "audio.sample_rate_hz", "48000"),
        NOW,
    );
    assert_eq!(error_code(&outcome), ErrorCode::Internal);
    assert_eq!(session.revision(), 0);
}

// ------------------------------------------------------------- idempotency --

#[test]
fn idempotency_ttl_boundary_is_sixty_seconds() {
    let mut session = session();
    let raw = set_json("t-1", 0, "audio.gain_db", "3.0");
    let first = send(&mut session, &raw, NOW);
    let replay = send(&mut session, &raw, NOW + 59_999);
    assert_eq!(first.response, replay.response);
    assert_eq!(session.revision(), 1);
    let expired = send(&mut session, &raw, NOW + 60_000);
    assert_eq!(error_code(&expired), ErrorCode::Conflict);
}

#[test]
fn idempotency_cache_is_fifo_with_64_entries() {
    let mut session = session();
    for i in 0..65 {
        let value = if i % 2 == 0 { "3.0" } else { "4.0" };
        let outcome = send(
            &mut session,
            &set_json(&format!("r{i}"), i, "audio.gain_db", value),
            NOW,
        );
        assert!(expect_ack(&outcome).applied);
    }
    assert_eq!(session.cached_responses(), 64);
    // r0 fue el más antiguo: ya no está, así que se reevalúa y choca.
    let evicted = send(
        &mut session,
        &set_json("r0", 0, "audio.gain_db", "3.0"),
        NOW + 1,
    );
    assert_eq!(error_code(&evicted), ErrorCode::Conflict);
    // r63 sigue cacheado: se reproduce su ack original con la revisión de entonces.
    let kept = send(
        &mut session,
        &set_json("r63", 63, "audio.gain_db", "4.0"),
        NOW + 2,
    );
    let ack = expect_ack(&kept);
    assert!(ack.applied);
    assert_eq!(ack.revision, 64);
}

#[test]
fn sessions_do_not_share_idempotency_cache() {
    let mut first = session();
    let mut second = session();
    let a = send(&mut first, &set_json("same", 0, "audio.mute", "true"), NOW);
    let b = send(&mut second, &set_json("same", 0, "audio.mute", "true"), NOW);
    assert!(expect_ack(&a).applied);
    assert!(expect_ack(&b).applied);
    assert_eq!(first.revision(), 1);
    assert_eq!(second.revision(), 1);
}

#[test]
fn revision_overflow_fails_closed_without_wrap_or_panic() {
    let mut session = ControlSession::new(
        peer(),
        ConfigState::with_revision(MAX_REVISION),
        Box::new(IdentityPolicy),
    );
    let outcome = send(
        &mut session,
        &set_json("c-1", MAX_REVISION, "audio.gain_db", "3.0"),
        NOW,
    );
    assert_eq!(error_code(&outcome), ErrorCode::Internal);
    assert_eq!(session.revision(), MAX_REVISION);
}

// ------------------------------------------------------------------ auth ----

#[test]
fn missing_capability_is_denied_before_any_state_change() {
    let mut session = ControlSession::new(
        AuthorizedPeer::new(
            "sess-0002",
            PeerRole::Android,
            vec![RemoteAction::ConfigRead],
        ),
        ConfigState::new(),
        Box::new(IdentityPolicy),
    );
    let outcome = send(&mut session, &set_json("c-1", 0, "audio.mute", "true"), NOW);
    assert_eq!(error_code(&outcome), ErrorCode::Denied);
    assert_eq!(session.revision(), 0);

    let mut read_only = ControlSession::new(
        AuthorizedPeer::new("sess-0003", PeerRole::Android, vec![]),
        ConfigState::new(),
        Box::new(IdentityPolicy),
    );
    let outcome = send(&mut read_only, &get_json("g-1", None), NOW);
    assert_eq!(error_code(&outcome), ErrorCode::Denied);
}

#[test]
fn microphone_and_permissions_have_no_remote_action() {
    for name in ["mic.start", "mic.stop", "permission.request"] {
        assert_eq!(classify_remote_action(name), Err(ErrorCode::Unsupported));
    }
    assert_eq!(
        classify_remote_action("config.read"),
        Ok(RemoteAction::ConfigRead)
    );
    assert_eq!(
        classify_remote_action("config.write"),
        Ok(RemoteAction::ConfigWrite)
    );
}

#[test]
fn hello_replies_capabilities_with_current_revision_and_rejects_other_major() {
    let mut session = session();
    let hello = r#"{"schema_version":"arl.control/v1","type":"hello","request_id":"h-1","payload":{"role":"android","version":"0.1.0","protocol_version":"arl.control/v1"}}"#;
    let outcome = send(&mut session, hello, NOW);
    match outcome.response.as_ref() {
        Some(ControlMessage::Capabilities(capabilities)) => {
            assert_eq!(capabilities.request_id, "h-1");
            assert_eq!(capabilities.revision, 0);
            assert_eq!(
                capabilities.capabilities.limits.max_control_payload_bytes,
                16384
            );
        }
        other => panic!("se esperaba capabilities: {other:?}"),
    }

    let wrong = r#"{"schema_version":"arl.control/v1","type":"hello","request_id":"h-2","payload":{"role":"android","version":"0.1.0","protocol_version":"arl.control/v2"}}"#;
    let outcome = send(&mut session, wrong, NOW + 1);
    assert_eq!(error_code(&outcome), ErrorCode::VersionUnsupported);
    assert!(outcome.close);
}

#[test]
fn ping_replies_pong_and_never_changes_revision() {
    let mut session = session();
    let outcome = send(
        &mut session,
        r#"{"schema_version":"arl.control/v1","type":"ping","request_id":"p-1","payload":{}}"#,
        NOW,
    );
    assert_eq!(
        outcome.response,
        Some(ControlMessage::Pong {
            request_id: "p-1".into()
        })
    );
    assert_eq!(session.revision(), 0);
}

#[test]
fn wrong_schema_version_is_rejected_with_request_id_echo() {
    let raw =
        br#"{"schema_version":"arl.control/v2","type":"ping","request_id":"p-1","payload":{}}"#;
    let error = wire::decode_message(&frame_len(raw)).unwrap_err();
    assert_eq!(error.code(), ErrorCode::VersionUnsupported);
    assert_eq!(error.request_id(), Some("p-1"));
}

#[test]
fn unknown_type_is_invalid_without_closing() {
    let mut session = session();
    let raw =
        r#"{"schema_version":"arl.control/v1","type":"mic.start","request_id":"m-1","payload":{}}"#;
    let outcome = send(&mut session, raw, NOW);
    assert_eq!(error_code(&outcome), ErrorCode::Invalid);
    assert!(!outcome.close);
}

// ----------------------------------------------------------- capabilities --

#[test]
fn capabilities_declare_contract_limits_and_all_scopes() {
    let capabilities = Capabilities::from_peer(
        "device-0001",
        PeerRole::LinuxDaemon,
        vec![RemoteAction::ConfigRead, RemoteAction::ConfigWrite],
    );
    assert_eq!(capabilities.limits.max_audio_datagram_bytes, 1200);
    assert_eq!(capabilities.limits.max_reorder_ms, 20);
    assert_eq!(capabilities.limits.max_config_keys, 32);
    assert_eq!(capabilities.limits.max_request_id_len, 64);
    assert_eq!(capabilities.config_keys.len(), 9);
    let mut scopes: Vec<&str> = capabilities
        .config_keys
        .iter()
        .map(|declaration| declaration.scope.as_str())
        .collect();
    scopes.sort_unstable();
    scopes.dedup();
    assert_eq!(scopes, vec!["local", "readonly", "shared"]);
    assert!(capabilities.features.contains(&Feature::AudioPcm16_48kMono));
    assert_eq!(
        capabilities
            .config_keys
            .iter()
            .find(|declaration| declaration.key.as_str() == "audio.gain_db")
            .unwrap()
            .value_type,
        ValueType::Number
    );
}

#[test]
fn capabilities_reject_unknown_features_actions_ids_and_extra_keys() {
    let base = |features: &str, actions: &str, device: &str| {
        format!(
            r#"{{"schema_version":"arl.control/v1","type":"capabilities","request_id":"c-1","revision":0,"payload":{{"schema_version":"arl.capabilities/v1","device_id":"{device}","role":"android","features":{features},"remote_actions":{actions},"limits":{{"max_control_payload_bytes":16384,"max_audio_datagram_bytes":1200,"max_reorder_ms":20,"max_config_keys":32,"max_request_id_len":64}},"config_keys":[]}}}}"#
        )
    };
    let good = base(r#"["control.v1"]"#, r#"["config.read"]"#, "device-0001");
    assert!(wire::decode_message(&frame_len(good.as_bytes())).is_ok());
    let bad_feature = base(r#"["mic.start"]"#, r#"["config.read"]"#, "device-0001");
    assert_eq!(
        wire::decode_message(&frame_len(bad_feature.as_bytes()))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    let bad_action = base(r#"["control.v1"]"#, r#"["mic.start"]"#, "device-0001");
    assert_eq!(
        wire::decode_message(&frame_len(bad_action.as_bytes()))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    let bad_id = base(r#"["control.v1"]"#, r#"["config.read"]"#, "short");
    assert_eq!(
        wire::decode_message(&frame_len(bad_id.as_bytes()))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
}

// ------------------------------------------------------------------ errors --

#[test]
fn error_catalog_matches_contract_retryable_and_close_flags() {
    for code in [
        ErrorCode::Invalid,
        ErrorCode::VersionUnsupported,
        ErrorCode::RequestIdReuse,
        ErrorCode::Conflict,
        ErrorCode::LocalKey,
        ErrorCode::Denied,
        ErrorCode::Unsupported,
        ErrorCode::Busy,
        ErrorCode::Unauthorized,
        ErrorCode::TooLarge,
        ErrorCode::NotFound,
        ErrorCode::Internal,
    ] {
        assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
    }
    assert!(ErrorCode::Conflict.retryable());
    assert!(ErrorCode::Busy.retryable());
    assert!(ErrorCode::Internal.retryable());
    assert!(!ErrorCode::Invalid.retryable());
    assert!(ErrorCode::Unauthorized.closes_connection());
    assert!(ErrorCode::VersionUnsupported.closes_connection());
    assert!(ErrorCode::TooLarge.closes_connection());
    assert!(!ErrorCode::Conflict.closes_connection());
    let long = "x".repeat(201);
    assert!(ErrorMsg::new("e-1", ErrorCode::Invalid, &long).is_err());
    let retryable_lie = br#"{"schema_version":"arl.control/v1","type":"error","request_id":"e-1","payload":{"code":"conflict","message":"x","retryable":false}}"#;
    assert_eq!(
        wire::decode_message(&frame_len(retryable_lie))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    let current_on_non_conflict = br#"{"schema_version":"arl.control/v1","type":"error","request_id":"e-1","payload":{"code":"invalid","message":"x","retryable":false,"current_revision":3}}"#;
    assert_eq!(
        wire::decode_message(&frame_len(current_on_non_conflict))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
}

#[test]
fn one_worker_applies_interleaved_sets_in_order() {
    let mut session = session();
    let a = send(&mut session, &set_json("a", 0, "audio.mute", "true"), NOW);
    let b = send(&mut session, &set_json("b", 0, "audio.mute", "false"), NOW);
    assert!(expect_ack(&a).applied);
    assert_eq!(error_code(&b), ErrorCode::Conflict);
    assert_eq!(session.revision(), 1);
    assert_eq!(
        session.snapshot().get(&key("audio.mute")),
        Some(&ConfigValue::Bool(true))
    );
}

#[test]
fn events_are_closed_and_session_state_detail_is_bounded() {
    let good = br#"{"schema_version":"arl.control/v1","type":"event","revision":1,"payload":{"name":"session.state","data":{"state":"closed","detail":"client closed"}}}"#;
    assert!(wire::decode_message(&frame_len(good)).is_ok());
    let extra = br#"{"schema_version":"arl.control/v1","type":"event","revision":1,"payload":{"name":"session.state","data":{"state":"closed","extra":1}}}"#;
    assert_eq!(
        wire::decode_message(&frame_len(extra)).unwrap_err().code(),
        ErrorCode::Invalid
    );
    let nested = br#"{"schema_version":"arl.control/v1","type":"event","revision":1,"payload":{"name":"config.changed","data":{"key":"audio.mute","requested":true,"effective":true,"x":{}}}}"#;
    assert_eq!(
        wire::decode_message(&frame_len(nested)).unwrap_err().code(),
        ErrorCode::Invalid
    );
    let long_detail = format!(
        r#"{{"schema_version":"arl.control/v1","type":"event","revision":1,"payload":{{"name":"session.state","data":{{"state":"closed","detail":"{}"}}}}}}"#,
        "x".repeat(201)
    );
    assert_eq!(
        wire::decode_message(&frame_len(long_detail.as_bytes()))
            .unwrap_err()
            .code(),
        ErrorCode::Invalid
    );
    assert_eq!(SessionState::Active.as_str(), "active");
}
