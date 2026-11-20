# `openrelay` v1 protocol — control and audio

**Status: Wire contract v1.** This document and the three schemas in [`contracts/`](../contracts/) remain the authority for what is sent, what the limits are, and what is rejected. The crates implement it; they do not extend it. Transport, identity and pairing are specified separately.

Scope: an authorized Android → Linux session. Audio travels in unreliable datagrams; control travels on a reliable stream. **This document does not define the codec, the TLS handshake, discovery or a remote administrative API.**

## 1. Terms

| Term | Meaning |
|---|---|
| Daemon | Linux process, sole authority for configuration and revision. |
| Client | Android (remote session) or local CLI (Unix socket). |
| Session | Authenticated and authorized connection from the client to the `session` endpoint. |
| Enrollment | Local window and `enroll` endpoint; it carries no audio or operational control. |
| `request_id` | Operation identifier for ACK, error and idempotency. |
| `revision` | Monotonic configuration counter of the daemon. |
| Epoch | Generation of an audio stream; changes when the audio path restarts or is reconfigured. |

## 2. Channels

| Channel | Transport | Reliability | Content |
|---|---|---|---|
| Control | Reliable stream (QUIC stream in the current transport) | Ordered delivery; explicit errors | JSON messages from this document |
| Audio | Datagrams (QUIC DATAGRAM in the current transport) | No retransmission; discard allowed | One PCM16 frame per datagram |

- Audio is **never** sent on the control stream, and control is never sent in audio datagrams.
- Control is **not** used to carry audio or to "recover" lost frames.
- The `enroll` endpoint only speaks enrollment messages; audio, control and tickets are rejected there (see [`security/pairing.md`](security/pairing.md)).

## 3. Control framing

Each control message occupies exactly one stream frame:

```text
+----------------+----------------------------------------+
| u32 BE length | length bytes of UTF-8 JSON |
+----------------+----------------------------------------+
```

| Field | Type | Limit |
|---|---|---|
| `length` | unsigned integer, big-endian | `1..=16384` |
| payload | UTF-8 JSON | up to 16384 bytes |
| total frame | — | `4 + length` ≤ 16388 bytes |

Encoding rules:

- UTF-8 without BOM; no compression; no concatenation of messages in one frame.
- No duplicate keys. Maximum JSON depth: **8**. At most **64** keys per object and **64** elements per array. Strings up to **1024** characters.
- Exact integers in the range `0..=9007199254740991` (2^53−1); floating-point numbers are not used for revisions, epochs, sequences or lengths.
- A frame with `length` out of range, invalid JSON, unknown type or a missing required field is rejected with `invalid`; if the framing is corrupt, the connection is closed.
- **v1 is strict:** objects do not allow additional properties. An incompatible extension requires `openrelay.control/v2` or an optional field declared in a later revision of the schema.

## 4. Control envelope

Every message is an object with this shape (schema: [`contracts/control-v1.schema.json`](../contracts/control-v1.schema.json)):

```json
{
  "schema_version": "openrelay.control/v1"
  "type": "config.set"
  "request_id": "8f14e45fceea167a"
  "expected_revision": 7
  "payload": { "key": "audio.gain_db", "value": 3.0 }
}
```

| Field | Type | Required | Limit / rule |
|---|---|---|---|
| `schema_version` | string | yes | constant `openrelay.control/v1` |
| `type` | string | yes | one of the types in §5 |
| `request_id` | string | requests and responses | `^[A-Za-z0-9_-]{1,64}$`; unique per client and session |
| `expected_revision` | integer | `config.set` only | `0..=2^53−1`; must equal the current revision |
| `revision` | integer | `ack`, `capabilities`, `event` | `0..=2^53−1`; resulting or current revision |
| `payload` | object | yes | depends on the type; max. 64 keys |

## 5. v1 messages

| Type | Direction | `request_id` | `revision` | `payload` |
|---|---|---|---|---|
| `hello` | client → daemon | yes | no | `{role, version, protocol_version}` |
| `capabilities` | both | yes | yes | capabilities object (§7) |
| `config.get` | client → daemon | yes | no | `{key: string \| null}` |
| `config.set` | client → daemon | yes | no (`expected_revision` yes) | `{key, value}` |
| `ack` | daemon → client | yes (echo) | yes | `{applied, requested, effective}` |
| `error` | daemon → client | yes (echo) | no | `{code, message, retryable, current_revision?}` |
| `event` | daemon → client | no | yes | `{name, data}` |
| `ping` / `pong` | both | yes | no | `{}` (max. 0 keys) |

### 5.1 `hello`

```json
{ "schema_version": "openrelay.control/v1", "type": "hello", "request_id": "h1"
  "payload": { "role": "android", "version": "0.2.1", "protocol_version": "openrelay.control/v1" } }
```

`role` ∈ `android`, `linux-cli`, `linux-daemon`. The daemon replies with `capabilities`. A different protocol version is rejected with `version_unsupported`.

### 5.2 `config.get`

`payload.key` is a specific key or `null` for the full snapshot. The response is an `ack` with `applied: true`, `requested: null` and `effective` containing the key or the full snapshot. `config.get` does not change the revision.

### 5.3 `config.set`

```json
{ "schema_version": "openrelay.control/v1", "type": "config.set", "request_id": "c-17"
  "expected_revision": 7
  "payload": { "key": "audio.gain_db", "value": 3.0 } }
```

Rules:

1. `expected_revision` must equal the daemon's current revision. Otherwise it returns `conflict` with `current_revision` and **nothing is applied**.
2. The key must exist and have `shared` scope. A `local` or `readonly` key is rejected with `local_key` / `denied` and is not applied.
3. The value is validated against the type and range in [`config-schema.md`](config-schema.md). An invalid value is rejected with `invalid` and is not applied.
4. If the effective value equals the current one, it returns `ack` with `applied: false` and the revision does not change.
5. If applied, the revision advances exactly **+1** and the `ack` returns `requested` and `effective`. `effective` may differ from `requested` because of device or OS limits; the client must show both.
6. The daemon emits a `config.changed` `event` with the new revision and `data` `{key, requested, effective}` (values, not pairs) to subscribed clients.

### 5.4 `ack`

```json
{ "schema_version": "openrelay.control/v1", "type": "ack", "request_id": "c-17", "revision": 8
  "payload": { "applied": true
               "requested": { "key": "audio.gain_db", "value": 3.0 }
               "effective": { "key": "audio.gain_db", "value": 3.0 } } }
```

`requested` is `null` in responses to `config.get` and in operations without a change payload. `effective` is never `null`.

### 5.5 `error`

```json
{ "schema_version": "openrelay.control/v1", "type": "error", "request_id": "c-17"
  "payload": { "code": "conflict", "message": "revision mismatch", "retryable": true
               "current_revision": 9 } }
```

`message` is short text (≤200 characters), with no secrets or internal paths. Full catalog in [`contracts/errors.md`](../contracts/errors.md).

### 5.6 `event`

Events from daemon → client. `payload.name` is `config.changed` or `session.state`; each name fixes a **closed** shape for `data`, with no additional keys and no nested objects or arrays. `data` contains no audio or secrets. An event requires no ACK or `request_id`. Reconnection uses snapshot + events; there is no indefinite replay.

`config.changed` is emitted after an applied `config.set`, with the new revision. `data.key` is the key; `requested` and `effective` are the requested and effective values, with the same per-key restrictions as `config.set` (they may differ because of device or OS limits).

```json
{ "schema_version": "openrelay.control/v1", "type": "event", "revision": 8
  "payload": { "name": "config.changed"
               "data": { "key": "audio.gain_db", "requested": 3.0, "effective": 3.0 } } }
```

`session.state` reports a session state change: `active` when authorized and `closed` when closed. `detail` is optional short text (≤200) with no secrets or internal paths.

```json
{ "schema_version": "openrelay.control/v1", "type": "event", "revision": 8
  "payload": { "name": "session.state"
               "data": { "state": "closed", "detail": "client closed" } } }
```

| Event | Required fields | Optional fields | Limits |
|---|---|---|---|
| `config.changed` | `key`, `requested`, `effective` | — | `key` 1..=64 with the pattern in §4; `requested`/`effective` per [`config-schema.md`](config-schema.md) |
| `session.state` | `state` | `detail` | `state` ∈ `active`, `closed`; `detail` ≤ 200 characters |

## 6. Revisions and idempotency

- The revision is an integer `0..=2^53−1`, owned exclusively by the daemon's control thread.
- It starts at `0` when the process starts; **monotonicity across restarts is not promised**. After reconnecting, the client fetches the snapshot and assumes nothing about its previous revision.
- Only an applied `config.set` increments the revision; `config.get`, `ping`, `event` and rejections do not change it.
- **Idempotency:** the daemon stores the last response (`ack` or `error`) per `request_id` in a per-session bounded cache:
  - at most **64** entries, oldest-first eviction;
  - TTL **60 s**;
  - same `request_id` + same canonical payload ⇒ the stored response is returned and **nothing is reapplied**;
  - same `request_id` + different payload ⇒ `error` with `request_id_reuse` (not retryable).
- The cache is lost when the session closes; the client must resend with `expected_revision` or re-read the snapshot. There is no global idempotency across sessions.
- Message order within the stream is application order. The daemon does not apply two `config.set` operations in parallel: there is a single control thread.

## 7. Capabilities

`capabilities` describes what each side may ask for and with which limits (schema: [`contracts/capabilities-v1.schema.json`](../contracts/capabilities-v1.schema.json)):

```json
{
  "schema_version": "openrelay.capabilities/v1"
  "device_id": "a1b2c3d4e5f60718"
  "role": "android"
  "features": ["control.v1", "audio.pcm16-48k-mono", "session.v1"]
  "remote_actions": ["config.read", "config.write"]
  "limits": {
    "max_control_payload_bytes": 16384
    "max_audio_datagram_bytes": 1200
    "max_reorder_ms": 20
    "max_config_keys": 32
    "max_request_id_len": 64
  }
  "config_keys": [
    { "key": "audio.gain_db", "scope": "shared", "type": "number", "minimum": -24, "maximum": 24 }
    { "key": "ui.theme", "scope": "local", "type": "string" }
  ]
}
```

- `remote_actions` only accepts `config.read` and `config.write`. There is **no** remote action to start or stop the microphone, grant permissions or change local keys.
- `mic.start`, `mic.stop` and `permission.request` are **local actions of the Android device**; they have no message type in v1. A client that asks for them over control receives `unsupported` (or `local_key` if the request is a configuration one).
- `config_keys` declares scope `shared`, `local` or `readonly`; a local key is not shared between devices.
- `audio.pcm16-48k-mono` is the base audio profile and must be announced to receive audio. `audio.opus-48k-mono` is optional: it enables the `type=2` profile only if **both** sides announce it; if either is missing, PCM16 is used (§8.3).
- `audio.return-pcm16-48k-mono` is optional and declares **return** audio Linux→Android (§8.4): it is only sent if **both** sides announce it and the local key `audio.return_enabled` is `true`.

## 8. Audio framing

An audio datagram contains **exactly one frame**. A **28-byte** big-endian header is followed by the payload. The base profile (`type=1`) uses PCM16LE mono with 480 bytes; the compatible `type=2` extension carries a variable Opus packet (§8.3):

| Offset | Bytes | Field | Type | Value / v1 limit |
|---:|---:|---|---|---|
| 0 | 1 | `type` | u8 | `1` (PCM16LE audio) or `2` (Opus audio; §8.3) |
| 1 | 1 | `version` | u8 | `1` |
| 2 | 1 | `flags` | u8 | bits 0–1; `0x01` silence, `0x02` discontinuity; bits 2–7 must be 0 |
| 3 | 1 | `header_len` | u8 | `28` |
| 4 | 4 | `epoch` | u32 BE | `1..=4294967295`; `0` is invalid |
| 8 | 4 | `seq` | u32 BE | `0..=4294967295`; +1 per frame, wrap allowed |
| 12 | 2 | `sample_count` | u16 BE | `240` in type=1; `120`, `240` or `480` in type=2 |
| 14 | 2 | `duration_us` | u16 BE | `5000` in type=1; `2500`, `5000` or `10000` in type=2, consistent with `sample_count` |
| 16 | 4 | `sample_rate_hz` | u32 BE | `48000` |
| 20 | 2 | `channels` | u16 BE | `1` |
| 22 | 2 | `reserved` | u16 BE | `0`; any other value ⇒ rejection |
| 24 | 4 | `payload_len` | u32 BE | `480` in type=1; `1..=1172` in type=2 |
| 28 | variable | payload | bytes | type=1: PCM16LE, 240 samples (480 bytes); type=2: one Opus packet (1..=1172 bytes) |

Total datagram: **508 bytes** for type=1; **29..=1200 bytes** for type=2. Maximum accepted size: **1200 bytes** (a conservative QUIC datagram budget). A larger datagram, one with `payload_len` different from `sample_count × channels × 2` (type=1) or from the bytes received (type=2), or one with different constants is rejected and counted; it is not fragmented at this level.

### 8.1 Epoch and sequence semantics

- `epoch` identifies a generation of the stream. It starts at `1` when the session is authorized and increments when audio restarts or the path changes in a way that alters format/clock. `0` is invalid.
- `seq` starts at `0` in each epoch and increments by 1 per frame; wrap to `0` is valid and is detected by epoch/time, not by naive comparison.
- A frame with an `epoch` lower than the current one is **old**: it is discarded, not played.
- A duplicate `seq` in the same epoch is discarded (it is not played twice).
- A `seq` gap is not recovered by retransmission: the receiver decides PLC/silence and counts it. Audio is never reordered beyond the reorder window (**20 ms**).
- `flags.silence` marks a silent payload; the payload is still present (480 bytes in type=1; 1..=1172 bytes in type=2). `flags.discontinuity` marks the first frame after a gap or epoch change; the receiver resets its jitter window.
- The sender does not send audio before the session is authorized. The receiver discards and counts any audio datagram received during `enroll` or before authorization.

### 8.2 Cadence and queues

- Nominal cadence: **200 frames/s** (5 ms) in the 240-sample profile; with 120 or 480 samples the cadence scales to keep 48,000 samples/s (§8.3). Receiver burst tolerance: 10 frames; maximum sustained rate 250 frames/s in the 5 ms profile.
- The sender keeps a very short output queue (≤ 2 frames) and purges before sending; old audio is discarded, never recovered.
- The receiver uses a bounded jitter buffer (target configurable 5–40 ms) and discards by deadline. The exact capacity limit is fixed in the implementation; the contract does not allow burst-driven growth.
- Clock drift is corrected in the consumer with a sample counter and slow resampling; Android/Linux monotonic clocks are not compared directly.

### 8.3 Negotiated Opus profile (type=2)

A backward-compatible v1 extension to carry Opus without changing the base profile. The header is the same 28 bytes; only the allowed values and the payload change:

| Field | Value in type=2 |
|---|---|
| `type` | `2` |
| `version`, `header_len`, `sample_rate_hz`, `channels`, `reserved` | same as type=1 (`1`, `28`, `48000`, `1`, `0`) |
| `flags` | same bits 0–1; bits 2–7 at 0 |
| `sample_count` | `120`, `240` or `480` |
| `duration_us` | `2500` with 120, `5000` with 240, `10000` with 480 |
| `payload_len` | `1..=1172` (1200 − 28) |
| payload | exactly **one Opus packet** (RFC 6716), opaque to the framing |

- The full datagram is `28 + payload_len`, i.e. **29..=1200 bytes**; `payload_len` must match the bytes received. A `payload_len` of `0` or greater than `1172`, a `sample_count` outside `{120,240,480}`, an inconsistent `duration_us` or any altered constant is rejected and counted, as in type=1.
- **Negotiation:** `type=2` is used only if **both** sides announce the `audio.opus-48k-mono` capability in `capabilities` (§7). If either is missing, the sender uses `type=1` PCM16LE and the receiver must not receive type=2. A receiver that gets type=2 without negotiation discards the datagram and counts it; there is no per-datagram control message.
- **Profile change:** switching from PCM16 to Opus or back increments `epoch`, resets `seq` to `0`, and the first frame carries `flags.discontinuity`. The receiver resets jitter and PLC with the new epoch.
- **Cadence:** 48,000 samples/s are maintained: 400 fps with 120 samples (2.5 ms), 200 fps with 240 (5 ms) and 100 fps with 480 (10 ms); the allowed burst remains 10 frames. `flags.silence` does not remove the payload: a silent frame still carries a valid Opus packet (DTX, if used, is an encoder decision).
- **Outside the contract:** bitrate, complexity, DTX/FEC, encoder pre-skip and any other internal Opus parameter.

The [`contracts/audio-frame-v1.schema.json`](../contracts/audio-frame-v1.schema.json) schema keeps the root descriptor for type=1 and describes type=2 in `$defs.audio_frame_opus`; `x-audio-profiles` lists both profiles and their capability.

### 8.4 Return audio Linux→Android (`type=1`)

Reverse direction, opt-in and off by default:

- It reuses the `type=1` frame from §8 unchanged: PCM16LE, 48 kHz, mono, 240 samples and 480 bytes of payload. There is no new profile or header.
- It is sent only if **both** sides announce `audio.return-pcm16-48k-mono` in `capabilities` **and** the daemon's local key `audio.return_enabled` is `true`. If either condition is missing, the daemon sends **not a single datagram** of return audio.
- Return audio is a stream independent of the Android→Linux microphone stream: it has its own `epoch` and `seq`, and is never mixed with the capture stream in the same datagram or epoch.
- The Android side must show an echo/feedback warning before enabling return audio; without that warning it is not enabled. Capturing audio from other Android apps is out of scope.
- How it is disabled and what is out of scope: [`return-audio.md`](return-audio.md).

## 9. Exact v1 limits

| Item | Limit |
|---|---|
| Control length prefix | 4 bytes BE |
| Control payload | 1..=16384 bytes |
| Total control frame | ≤ 16388 bytes |
| JSON depth | 8 |
| Keys per object / elements per array | 64 / 64 |
| JSON string | 1024 characters |
| Events | `config.changed`/`session.state`; `data` closed and flat (no nested objects or arrays) |
| `request_id` | 1..=64 characters `[A-Za-z0-9_-]` |
| Idempotency cache | 64 entries, TTL 60 s, oldest-first, per session |
| Audio header | 28 bytes |
| Audio payload | 480 bytes (240 samples × 1 channel × 2 bytes) |
| Samples per Opus frame | 120, 240 or 480 (2.5, 5 or 10 ms at 48 kHz) |
| Opus audio payload | 1..=1172 bytes (exactly one Opus packet) |
| Audio datagram | 508 bytes; maximum accepted 1200 |
| Opus audio datagram | 29..=1200 bytes; maximum accepted 1200 |
| Frames per datagram | 1 |
| Reorder window | 20 ms |
| Cadence / burst | 200 fps nominal; 250 fps sustained; burst 10 |
| Jitter target | 5–40 ms (configurable; default 10 ms) |

These limits are part of the contract: the implementation must enforce and test them. A different limit requires a contract revision, not a "reasonable" value in code.

## 10. Versioning and compatibility

- The major lives in `schema_version` (`openrelay.control/v1`, `openrelay.capabilities/v1`, `openrelay.audio-frame/v1`). An unknown major is rejected with `version_unsupported`; no attempt is made to interpret it.
- v1 does not accept unknown fields. Compatible additive changes are declared explicitly in a documented schema revision; incompatible ones open v2.
- The base audio profile is fixed PCM16LE mono 48 kHz. The compatible `type=2` (Opus) extension is negotiated with the `audio.opus-48k-mono` capability (§8.3); if it is missing on either side, PCM16 is used. Any other codec or wire parameter requires a new contract revision.
- The session validates version and limits **before** accepting audio or operational control.

## 11. What this contract does not define

- TLS handshake, SPKI pin, client identity, mTLS/Keystore or app-layer: [`security/pairing.md`](security/pairing.md) and the ADRs.
- Concrete transport (quinn), mDNS discovery and ports: [ADR-0001](adr/0001-transport.md).
- Internal Opus parameters (bitrate, complexity, DTX/FEC, pre-skip), DSP/noise reduction and latency metrics: [`codec.md`](codec.md), [ADR-0003](adr/0003-ns.md) and [`latency-budget.md`](latency-budget.md).
- CLI and Unix socket API: [`cli-quickstart.md`](cli-quickstart.md); this document only assumes the daemon is the configuration authority.
