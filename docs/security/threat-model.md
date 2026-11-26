# Threat model — `openrelay` v1

**Status: Design constraints, not a passed security review.**

Scope: Android microphone audio → Linux over the LAN, bidirectional control of configuration and state, local enrollment and authenticated sessions. Out of scope: Internet exposure, capture of audio from other apps, return audio (opt-in, not available in 0.3.1) and any remote administrative API.

## 1. Assets

| ID | Asset | Why it matters |
|---|---|---|
| A1 | Android microphone audio | Real-time personal data; interception or injection is the central threat. |
| A2 | Configuration and session state | Decides what is captured, at what volume and whether the microphone is active. |
| A3 | Persistent daemon and client identity | It is the anchor for later sessions; impersonation breaks all trust. |
| A4 | Enrollment window, SPKI pin and single-use token | It provisions identity; a leaked or reused token enrolls a foreign device. |
| A5 | ACL, capabilities and tickets | They decide what each identity may do; default deny. |
| A6 | Logs, metadata and counters | They must not leak tokens, keys, audio or unnecessary identifiers. |
| A7 | Local control socket | Improper access allows changing configuration and stopping sessions. |
| A8 | Build and release artifacts | A tampered binary or APK defeats every other control. |

## 2. Trust boundaries

| ID | Boundary | What crosses it |
|---|---|---|
| B1 | Android app ↔ LAN | Audio, control and session messages leave the device. |
| B2 | LAN ↔ daemon `enroll` endpoint | Client without a certificate, token, pin proof. |
| B3 | LAN ↔ daemon `session` endpoint | Client identity, audio and operational control. |
| B4 | CLI ↔ daemon over Unix socket | Local operations authenticated by UID. |
| B5 | Daemon ↔ PipeWire / OS | `Audio/Source` node, permissions and devices. |
| B6 | Daemon ↔ on-disk state | Identity, ACL and configuration; no unnecessary plaintext secrets. |

**Boundary rule:** `enroll` and `session` are **separate endpoints** with disjoint message sets and different ALPN. The enrollment endpoint never carries audio or operational control. The pin from the enrollment QR is the server identity the phone stores and later requires for sessions; it is not a client credential and never substitutes for the client certificate.

## 3. Adversaries considered

1. Passive observer on the LAN (sniffing, metadata).
2. Active attacker on the LAN (MITM, ARP/ND spoofing, fake DNS/mDNS, replay).
3. Malicious app or user on the same LAN that tries to connect or flood.
4. Malicious or compromised paired peer that already holds an identity.
5. Thief with an unlocked Android device or the Linux machine.
6. Unprivileged local user on the Linux machine.
7. Availability attacker (connection, message or datagram flood).
8. Physical observer of the QR or the terminal during enrollment.
9. Compromised dependency or build artifact.

## 4. Security invariants

- **I1.** Audio is never delivered before the session is authorized.
- **I2.** The enrollment endpoint accepts no audio, operational control or tickets.
- **I3.** The session endpoint requires a valid client identity: mTLS with explicit `peer_cert()` + ACL, or a reviewed standard app-layer scheme bound to the TLS channel. `verify_peer(true)` **does not** replace the `peer_cert()` check.
- **I4.** There is no silent TOFU: an unknown identity is neither accepted nor remembered without explicit local approval.
- **I5.** No `verify=false`, `danger_accept_invalid_certs` or equivalent exists in release builds; there is no automatic fallback to cleartext UDP and no 0-RTT.
- **I6.** The remote channel grants no Android permissions, does not start the microphone and does not write local keys.
- **I7.** Every parser and every queue has size, depth and rate limits; nothing allocates memory proportional to input.
- **I8.** Revoking an identity closes its sessions and invalidates its tickets.

## 5. Threats and treatment

| Threat | Surface | Mitigation | Residual risk |
|---|---|---|---|
| Sniffing of audio or control on the LAN | B1/B3 | TLS 1.3/QUIC with encryption and authentication; no cleartext UDP; no downgrade | Volume/duration metadata visible to a network observer; accepted on the LAN. |
| MITM during enrollment to steal the token | B2 | The client validates the SPKI pin of the server certificate **before** sending the token; short TTL; single-use token; local approval with fingerprint and short code | A user who approves without comparing the code; mitigated by the UI and accepted as human error. |
| Daemon impersonation in a session | B3 | Persistent server identity and trust anchor; the client rejects a different anchor | Compromise of the Linux machine; outside the model. |
| Client without a certificate accepted as a session | B3 | `peer_cert() == None` is rejected before control/audio; ACL default deny; mandatory negative test | A future implementation could misread `verify_peer`; covered by negative tests. |
| TOFU: accepting and remembering an unapproved identity | B2/B3 | No automatic enrollment; identity only after local approval; lost state ⇒ re-enroll, never silently accept a new server/client | A user who re-enrolls without verifying; recorded in the local log. |
| Replay or reuse of the enrollment token | B2 | 256-bit random token, single use, TTL ≤300 s, bound to the window; invalidated on approval, cancellation or expiry; never logged | Physical capture of the QR during the window; TTL and local confirmation bound it. |
| Audio or control injected into the enrollment endpoint | B2 | The enrollment server only speaks enrollment messages; audio/control/ticket are rejected with an error and close; the audio receiver discards every datagram outside an authorized session | An implementation bug; covered by enrollment boundary tests. |
| Malicious app on the LAN tries to connect to `session` | B3 | Mandatory client identity + ACL; rate limit per IP/identity; rejection does not reveal whether the identity exists | Resource brute force; bounded by quotas and monitored. |
| Malicious paired peer asks for more than allowed | B3 | Capabilities and ACL per operation; local keys rejected; no shell, files, HTTP or elevation; only the Android user starts the microphone | The peer can exhaust its own control quota; it is limited and the session is closed. |
| Thief uses an already paired device | B1/B3 | Revocation by identity closes the session and invalidates tickets; short-lived tickets; re-enroll after revoking; device lock is Android's responsibility | Window until the user revokes; documented. |
| Unprivileged local user controls the daemon | B4 | Socket at `$XDG_RUNTIME_DIR/openrelay/control.sock`, directory `0700`, socket `0600`, `SO_PEERCRED` with owner UID; no administrative TCP | Processes with the same UID; accepted (same user). |
| Flood of connections, messages or datagrams | B2/B3 | Limits before and after the handshake: connections per IP, attempt rate, maximum size, depth, strings, frames per datagram, timeouts and per-identity quotas; bounded queues with discard | Volumetric DoS on the network or host; outside the application's control. |
| Malicious parser causes panic, OOM or quadratic cost | B2/B3 | Exact limits from [`docs/protocol.md`](../protocol.md); rejection without proportional allocation; fuzzing of control and datagram in release validation | Unknown bug; fuzzing is a release requirement. |
| Token, key or audio leak in logs | A6/B6 | Token neither written to disk nor included in status JSON; logs without secrets; identity material in protected state | Third-party debug logs; prohibited in code and reviewed. |
| QR observed by a nearby third party | B2 | `openrelay pair` only in a TTY, short TTL, single-use token, local approval; the link is shown in the terminal and not written to disk | Camera at the exact moment; the short code and local approval bound it. |
| Compromised dependency or build artifact | A8 | Dependencies pinned and recorded with license and provenance; reproducible builds and SBOM; no opaque binaries; license review | Undetected upstream compromise; reduced by pinning and audit, not eliminated. |
| Microphone active without the user's knowledge | B1/B5 | Foreground service with notification and local stop; start from visible UI; process death leaves no orphaned microphone; the remote cannot start it | OEM/OS bug; tested on supported Android versions. |
| PipeWire node exposed after the session ends | B5 | The node stops producing when capture stops; no `Audio/Source` is published without an active session; local stop and `devices remove` closes the session | Empty visible node during the transition; accepted and documented. |

## 6. Accepted residual risks

1. **Human approval error** during enrollment (comparing fingerprint/code is the user's responsibility).
2. **Volumetric DoS** at network/host level, outside the application's scope.
3. **Compromise of the OS or of an already paired device**; revocation is the response, not prevention.
4. **Android OEM bugs** in foreground services, microphone or Keystore; tested on specific devices.
5. **Network metadata** (IP, ports, traffic volume, session duration).
6. **License uncertainty around RNNoise weights** and DFN artifacts; they are not distributed until resolved.
7. **Keystore limitation** if there is no signing bridge: it is declared in ADR-0002, and a non-exportable hardware key is not promised.

## 7. Assumptions and limits

- The LAN is considered hostile; the model **does not** assume a trusted home network.
- The Linux daemon is not exposed to the Internet. If the user does so, it is out of support and must be documented as their own risk.
- Audio is PCM16 mono 48 kHz in v1; the exact contract is in [`docs/protocol.md`](../protocol.md).
- Client identity, the enrollment handshake, the exact pairing message format and key storage are defined in their ADRs; this document sets requirements and prohibitions.
- There is no implementation audit yet. The negative identity, enrollment-boundary and pairing tests live in the test suites.
