# Configuration v1 — requested/effective and local keys

The app exposes mute and a gain slider. Gain and mute are applied by the daemon's audio path: gain scales the PCM16 samples published on the virtual microphone, and PC-side mute cuts that microphone. Return audio is opt-in: it needs the `return-audio` Cargo feature (off by default), a `return_audio` grant and `audio.return_enabled = true`.

## 1. Model

- Every operation carries `request_id`, and `config.set` requires `expected_revision`.
- The daemon answers `ack` with three separate things:
  - **`requested`**: what the client asked for;
  - **`effective`**: what the daemon actually applied (it may differ because of device/OS limits);
  - **`revision`**: the resulting revision.
- `applied: true` means the state changed and the revision advanced by +1. `applied: false` means the effective value was already the current one; the revision does not change.
- A revision conflict applies nothing and returns `error` with `conflict` and `current_revision`.
- There is no partial application: if a key or a value is invalid, the whole operation is rejected.
- The revision is **per daemon process**; monotonicity across restarts is not promised. On reconnect, the client asks for the snapshot (`config.get` with `key: null`).

## 2. v1 keys

| Key | Type | Range / values | Default | Scope | Notes |
|---|---|---|---|---|---|
| `audio.sample_rate_hz` | integer | `48000` (constant in v1) | `48000` | shared | The remote end can read it; v1 has no other rate. `effective` may reflect the device's real rate. |
| `audio.frame_duration_ms` | integer | `5` (constant in v1) | `5` | shared | One frame = 240 samples at 48 kHz. |
| `audio.gain_db` | number | `-24.0..=24.0`, multiple of `0.5` | `0.0` | shared | Capture gain; the effective value may be limited by the device. |
| `audio.mute` | boolean | `true` / `false` | `false` | shared | Mutes the stream; it does not stop the microphone or the FGS. |
| `audio.jitter_target_ms` | integer | `5..=40` | `10` | shared | Target of the Linux receiver's jitter buffer. |
| `audio.return_enabled` | boolean | `true` / `false` | `false` | **local** | Enables Linux→Android audio return; off by default. See [`return-audio.md`](return-audio.md). |
| `ui.theme` | string | `system`, `light`, `dark` | `system` | **local** | Device visual preference; not shared. |
| `ui.text_scale` | number | `1.0..=2.0` | `1.0` | **local** | Local accessibility; not shared. |
| `device.local_volume` | number | `0.0..=1.0` | `1.0` | **local** | Device local volume; it is not stream gain. |
| `device.model` | string | ≤64 characters | — | **readonly** | Informational; the remote end cannot write it. |

Rules:

- **local** and **readonly** keys are never accepted over the remote channel: `config.set` on them returns `local_key` (local) or `denied` (readonly) and applies **nothing**. The Android panel manages them in its own storage; the local CLI can read them if the daemon exposes them, but they are not part of the shared configuration.
- `mic.start`, `mic.stop` and `permission.request` are **not configuration keys** and not remote actions. They are local actions of the Android device; no control message executes them in v1.
- `audio.gain_db` is not `device.local_volume`: the first affects the captured stream; the second is local playback and is not shared.
- `audio.return_enabled` is **local to the daemon** and is not shared: the Android panel does not write it over the remote channel. Return also requires both ends to advertise the `audio.return-pcm16-48k-mono` feature; without both conditions the daemon sends no return audio (§ [return-audio.md](return-audio.md)). The ACL side is `openrelay devices grant <fingerprint> return`, and the daemon must be built with the `return-audio` Cargo feature, which is off by default.
- The effective value of `audio.sample_rate_hz` and `audio.frame_duration_ms` is published for diagnostics; in v1 it must be 48000 and 5. If the device cannot, the session is not authorized with that format (the contract does not invent a silent resample).

## 3. Exchange example

```json
{ "schema_version": "openrelay.control/v1", "type": "config.set", "request_id": "c-17",
  "expected_revision": 7,
  "payload": { "key": "audio.gain_db", "value": 3.0 } }
```

```json
{ "schema_version": "openrelay.control/v1", "type": "ack", "request_id": "c-17", "revision": 8,
  "payload": { "applied": true,
               "requested": { "key": "audio.gain_db", "value": 3.0 },
               "effective": { "key": "audio.gain_db", "value": 3.0 } } }
```

Revision conflict:

```json
{ "schema_version": "openrelay.control/v1", "type": "error", "request_id": "c-18",
  "payload": { "code": "conflict", "message": "revision mismatch", "retryable": true,
               "current_revision": 8 } }
```

Local key:

```json
{ "schema_version": "openrelay.control/v1", "type": "error", "request_id": "c-19",
  "payload": { "code": "local_key", "message": "key is local to the device", "retryable": false } }
```

## 4. Local file

Proposal: `$XDG_CONFIG_HOME/openrelay/config.toml`, no secrets, atomic writes and user permissions.

```toml
# Shared configuration and local preferences of the Linux daemon only.
[audio]
sample_rate_hz = 48000
frame_duration_ms = 5
gain_db = 0.0
mute = false
jitter_target_ms = 10
return_enabled = false

[ui]
theme = "system"
text_scale = 1.0
```

- Unknown keys or values out of range ⇒ the daemon does not start with that configuration and reports the error; nothing is ignored silently.
- The file contains no enrollment token, private keys, ACL or identity material. Those live in the daemon's protected state, not in `config.toml`.
- Defaults are applied only when the key is missing; an invalid value is never silently replaced by the default.

## 5. Validation and errors

| Situation | Result |
|---|---|
| Key does not exist | `invalid` (not retryable) |
| local/readonly key over the remote channel | `local_key` / `denied` (not retryable) |
| Wrong type | `invalid` (not retryable) |
| Value out of range or not a multiple of 0.5 in `gain_db` | `invalid` (not retryable) |
| `expected_revision` differs | `conflict` (retryable after re-reading) |
| `request_id` reused with another payload | `request_id_reuse` (not retryable) |
| Value equal to the current effective value | `ack` with `applied: false`, no revision change |

The full catalogue and the mapping to CLI exit codes are in [`contracts/errors.md`](../contracts/errors.md).

## 6. Limits

- At most **32** keys declared in `capabilities.config_keys`.
- `request_id` from 1 to 64 characters; `key` from 1 to 64 characters matching `^[a-z][a-z0-9_]*(\.[a-z0-9_]+){0,3}$`.
- Value strings ≤1024 characters; numbers within the range declared per key.
- The idempotency cache holds 64 entries for 60 s per session (see [`protocol.md`](protocol.md) §6).
