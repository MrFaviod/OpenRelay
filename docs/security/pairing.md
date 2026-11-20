# Pairing and session security

**Status: implemented since v0.2.0, current release v0.2.2 (adds `openrelay devices remove`).** This document describes what the daemon, the CLI and the Android app actually do. Where the implementation is weaker than the original design, it says so.

## 1. Two surfaces

| Surface | Server certificate | Client certificate | Accepts | Never accepts |
|---|---|---|---|---|
| **Enrollment** (`47001/udp`, only while `openrelay pair` runs) | The daemon's persistent session certificate, pinned by SPKI in the QR | None | `enroll.request` with token and client SPKI, then `enroll.result` | Audio, control, tickets |
| **Session** (`47000/udp`) | The same persistent certificate, validated against the local CA and the pinned SPKI | Yes: `clientAuth` leaf signed by the local CA, plus ACL | Audio and control | A client without a valid identity or grant |

- The surfaces use different ALPNs (`openrelay-enroll/1`, `openrelay-session/1`) and disjoint message sets. The phone connects to the session endpoint after approval; there is no in-band transition from enrollment to session.
- Enrollment accepts clients without a certificate. Such a connection is not a session and grants no audio or control.
- The enrollment endpoint is bound to `0.0.0.0:47001` and exists only while a window is open. It is not a long-running listener.

### Deliberate simplification: one certificate, not one per window

The original design called for an ephemeral enrollment certificate per pairing window. The implementation reuses the daemon's **persistent** server certificate for enrollment and for sessions:

- fewer moving parts and no second key lifecycle in the daemon;
- the QR pin is `SHA-256(SubjectPublicKeyInfo)` of that persistent certificate;
- the session client trusts the same local CA and additionally pins the server SPKI.

The cost is real: the pin is not fresh per window, so a pin captured from an old QR still identifies the same server, and compromise of the server key affects enrollment and sessions alike. It does **not** let an attacker enroll: that still requires the one-time token, a client key, and a human `y` on the PC. A future version may move to a per-window certificate; until then this is the documented trade-off.

## 2. Window and QR

`openrelay pair` calls `pair.open` over the local Unix socket. The daemon:

1. takes the single enrollment slot (a second call answers `busy`);
2. binds `0.0.0.0:47001` and creates a fresh 32-byte **single-use token**, valid for `--ttl` seconds (1-300, default 120);
3. builds the URI:

```text
openrelay://pair?v=1&h=<ip>:47001&pin=<64 hex>&t=<64 hex>
```

`h` is the advertised address: the primary LAN IPv4 by default, or the literal IP from `--advertise`. `pin` is the SPKI pin of the server certificate. `t` is the token. The URI is printed to the terminal together with a QR; it is never written to disk, logged, or sent anywhere else.

The token lives in memory only. It is compared in constant time and invalidated on first use, on expiry, or when the window closes.

## 3. Enrollment exchange

1. The phone scans the QR (or pastes the URI) and connects to `h` over QUIC with ALPN `openrelay-enroll/1`, **without** a client certificate.
2. It validates the server's SPKI pin **before** sending anything. A mismatch aborts the connection without revealing the token; there is no `verify=false` path.
3. It sends:

```json
{"type":"enroll.request","version":1,"token":"<64 hex>","spki":"<client public key, hex DER>"}
```

4. The server checks the token, window and message limits, computes `SHA-256(spki)` — the device fingerprint — and queues the device as **pending**. Nothing is persisted yet. The phone shows *Waiting for approval on the PC…*.
5. The CLI polls `pair.pending`, prints `Device: <fingerprint>` and asks `Approve this device? [y/N]`. `--yes` answers the first prompt automatically.
6. On `y`, the daemon signs a `clientAuth` leaf for the device key with the local CA, adds the pin to the transport ACL and to `identities.json`/`grants.txt`, and sends the result back over the enrollment stream:

```json
{"type":"enroll.result","version":1,"status":"accepted",
 "client_leaf":"<hex DER>","session_ca":"<hex DER>","session_port":47000}
```

7. The phone stores the leaf, the CA, the server pin and the host, then connects to the session endpoint. The window stays open a few seconds so the result survives packet loss, then closes.
8. On `n`, EOF, Ctrl-C, expiry or a QR failure the window closes without writing anything. A pending phone receives `denied` when the close is orderly.

The QR is not persisted and carries no identity, private key or configuration. The token is the only secret in it, and it is useful only while the window is open and only from the LAN.

## 4. Session

1. The app opens QUIC to `h:47000` with ALPN `openrelay-session/1`, presenting its `clientAuth` leaf.
2. The server requires a client certificate, validates it against the local CA, and looks up the device pin in the ACL. Default deny: a certificate that is valid but absent from `identities.json` is rejected before any audio or control.
3. TLS is validated by the app against the stored CA **and** the pinned server SPKI, using the fixed server name `openrelay` (not the QR IP). The server name is stable across address changes.
4. Only then does the audio path open: the phone sends encrypted QUIC datagrams and the daemon decodes them into `openrelay-mic`. Audio before authorization is discarded.

Session grants are per device: `mic`, `control_read`, `control_write` (and optionally `return_audio`). `openrelay devices` prints them.

## 5. Rejections

| Case | Result |
|---|---|
| Token reused, expired, or from another window | Rejected; no identity is written; the attempt is counted without the token |
| SPKI pin mismatch | The client aborts before sending the token |
| Client with no certificate or unknown identity on the session endpoint | Rejected before control or audio |
| Audio, control or tickets on the enrollment surface | Rejected and the connection is closed |
| Local approval denied | Window destroyed; no identity, no ACL entry |
| Lost local state | The device must enroll again; there is no TOFU and no silent re-enrollment |
| Oversized or malformed enrollment message | Rejected by size limits before deep parsing |
| Second `pair.open` while a window is open | `busy`; the first window is untouched |

## 6. Identity, ACL, revocation

- Identity is per device, not per connection. The ACL is indexed by SPKI pin and capability. Default deny.
- `identities.json` is the persisted approval list; `grants.txt` mirrors the control grants. Both are mode 0600 in the state directory, as are the CA and server keys.
- Removing a device with `openrelay devices remove` takes it out of `identities.json` and the running ACL, and closes its live session at once. It does not erase the audit trail of the approval.
- If the daemon state is regenerated, the server pin changes and every phone must pair again. There is no silent anchor rotation.
- **Trust limit:** local approval is human. The fingerprint reduces operator error; it does not eliminate it.

## 7. Limits and assumptions

- One window per daemon, one enrolled client per window, TTL ≤ 300 s.
- Enrollment carries no audio and no control: there is nothing to protect with a session before approval.
- The LAN is assumed hostile: the flow does not rely on the network being trustworthy, only on the QR channel and the local approval.
- The Android app keeps the client key in its own keystore-backed signer; the private key never leaves the phone. The leaf is public and is stored with the pairing.
- Message names and encodings are fixed by `contracts/` and exercised by the enrollment and pairing tests.

## 8. Related documents

- Assets, adversaries and residual risk: [`threat-model.md`](threat-model.md).
- Control/audio contract and limits: [`../protocol.md`](../protocol.md).
- Shared configuration and local keys: [`../config-schema.md`](../config-schema.md).
- CLI flow: [`../cli-quickstart.md`](../cli-quickstart.md).
