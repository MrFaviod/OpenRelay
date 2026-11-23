# `openrelay` CLI: quick guide

One binary does two jobs. It is the **daemon** (`openrelay daemon`) and the **client** that talks to it over a local Unix socket. Every command that talks to the daemon prints JSON on stdout, so it pipes cleanly into `jq`.

```text
openrelay daemon        [--socket PATH] [--quic ADDR | --no-quic] [--state-dir PATH]
                        [--identity-der PATH --identity-key PATH --client-ca PATH
                         --client-ca-key PATH --identities PATH --control-grants PATH]
openrelay status        [--socket PATH]                    daemon state
openrelay capabilities  [--socket PATH]                    keys, ranges, current revision
openrelay config get    [KEY] [--socket PATH]              read one key, or all of them
openrelay config set    KEY VALUE --rev N [--socket PATH]  write one key
openrelay pair          [--ttl 120] [--advertise IP] [--yes] [--socket PATH]
                                                           QR + local approval of a phone
openrelay devices       [--state-dir PATH]                 list approved identities
openrelay devices remove FINGERPRINT [--socket PATH]       revoke a device and close its sessions
openrelay service print-unit                               print the systemd --user unit
openrelay --help | openrelay <command> --help
```

## 1. Start the daemon

```sh
openrelay daemon
```

If `openrelay status` says `the daemon is not running`, start it:

```sh
systemctl --user daemon-reload && systemctl --user enable --now openrelay.service
```

(or run `openrelay daemon`). To update later, run `curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | sh -s -- --update`; pairings and settings are kept.

It stays in the foreground until Ctrl-C or SIGTERM, and never forks; there is no `--foreground` flag. A socket left behind by a killed process is removed on the next start, while a live one is an error. To run it in the background, use the user unit:

```sh
systemctl --user enable --now openrelay.service
```

On first start the default daemon **provisions its own identity** in `${XDG_CONFIG_HOME:-~/.config}/openrelay/` and opens the session endpoint on `0.0.0.0:47000` (UDP). `--state-dir DIR` moves the state directory; `--no-quic` starts only the local control socket and creates no identity or files; `--quic ADDR` changes the bind address/port; `--tailscale` binds sessions and enrollment to the IPv4 that `tailscale ip -4` reports, so nothing listens on the LAN. Passing the six `--identity-*` / `--client-ca*` / `--identities` / `--control-grants` flags uses an existing provision instead of generating one (all six are required together).

| File | Contents | Mode |
|---|---|---|
| `server.der` | daemon session certificate (also used for enrollment) | `0600` |
| `server.key.der` | its PKCS#8 private key | `0600` |
| `client-ca.der` | CA that signs per-device client certificates | `0600` |
| `client-ca.key.der` | CA private key, used at approval time | `0600` |
| `identities.json` | approved devices and capabilities | `0600` |
| `grants.txt` | control grants per pin | `0600` |

The control socket lives at `$XDG_RUNTIME_DIR/openrelay/control.sock`. Its directory is mode 0700 and the socket is 0600. It only accepts clients with the **same UID** (`SO_PEERCRED`), and it never listens on TCP.

The daemon needs inbound UDP **47000** (sessions) and, while a pairing window is open, **47001** (enrollment). Open both in the host firewall; see [INSTALL.md](../INSTALL.md#firewall). With `--tailscale` there is nothing to open: both ports are bound to the Tailscale IP, which the tailnet already reaches.

## 2. Check it

```sh
openrelay status
openrelay status | jq .
```

```json
{"ok":true,"request_id":"cli-1234-…","result":{
  "version":"0.2.2","uptime_s":42,"sessions":1,
  "quic":"0.0.0.0:47000",
  "pipewire":{"ok":true},
  "pair":{"open":false,"pending":0,"expires_at":null}}}
```

| Field | Meaning |
|---|---|
| `version` | daemon version (`0.2.2` in this release) |
| `uptime_s` | seconds since the daemon started |
| `sessions` | authorized sessions alive right now (one per streaming phone) |
| `quic` | session bind address, or `null` when `--no-quic` |
| `pipewire.ok` | `true` when the `openrelay-mic` source exists; otherwise `error` says why |
| `pair.open` | a pairing window is open |
| `pair.pending` | devices waiting for local approval |
| `pair.expires_at` | window expiry as a Unix timestamp, or `null` |

## 3. Pair a phone

```sh
openrelay pair
```

The daemon opens a single enrollment window, prints the one-time `openrelay://pair?v=1&h=…&pin=…&t=…` URI and a QR in the terminal, then polls for the phone. When a device enrolls, the CLI prints its fingerprint and asks:

```text
Device: 9f2c…
Approve this device? [y/N]
```

`y` signs a `clientAuth` leaf for the device key and hands it back over the enrollment stream; the phone then connects to the session endpoint. `n` rejects it. Flags:

| Flag | Effect |
|---|---|
| `--ttl N` | window length in seconds, `1`–`300` (default `120`) |
| `--advertise IP` | address written into the QR; an IP literal, not a hostname. Defaults to the daemon's bound IP (the Tailscale IP with `--tailscale`), otherwise the PC's primary LAN IPv4 |
| `--yes` | approve the first device without prompting |

The window closes on rejection, expiry, Ctrl-C, EOF on stdin or a QR failure, so the next `openrelay pair` is never blocked by a stale window; the daemon also drops a window whose CLI process died. After approval it stays open for a few seconds so the phone can receive its certificate. Only one window can exist at a time; a second attempt answers `busy`.

List what is approved without talking to the daemon:

```sh
openrelay devices
# 9f2c…  mic=yes control_read=yes control_write=yes return_audio=no
```

With no approved device it prints `openrelay: no approved devices` on stderr and exits 0.

The list is read from `identities.json`. A device absent from it is denied by the ACL, even if it completed enrollment.

Revoke a device with `openrelay devices remove <fingerprint>`. It also closes any live session for that device.

## 4. `audio.stats`

`audio.stats` is a daemon operation, not a subcommand: it reports per-session counters and is meant for scripts. Send one JSON line over the control socket (any same-UID client works):

```sh
printf '%s\n' '{"request_id":"stats-1","op":"audio.stats"}' |
  socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/openrelay/control.sock"
```

```json
{"ok":true,"request_id":"stats-1","result":{"sessions":[
  {"session":1,"pin":"9f2c…","pcm16_frames":2880,"opus_frames":0,
   "plc_frames":0,"dropped_duplicates":0,"dropped_reordered":0,
   "dropped_cadence":0,"discontinuities":1,"epoch_resets":1,
   "decode_errors":0,"ns_stages":0,"ns_frames_processed":0,
   "ns_frames_bypassed":2880,"ns_added_latency_samples":0}]}}
```

`pcm16_frames` climbing while `decode_errors` and `dropped_*` stay at zero is the quickest proof that audio is arriving. `sessions: []` means no phone is connected.

The control socket takes one JSON line per request and returns one line per response:

```json
{"request_id":"cli-1","op":"audio.stats"}
```

Operations are `status`, `config.get`, `config.set`, `capabilities` and `audio.stats`. Success returns `{"request_id":…,"ok":true,"result":…}`, where `result` is the v1 `ack`, `capabilities` or `error`. An operation error returns `{"request_id":…,"ok":false,"error":{"code","message","retryable"[,"current_revision"]}}`. Reusing a `request_id` with the same operation returns the stored response for 60 s; reusing it with another payload returns `request_id_reuse`. Only clients with the same UID are accepted (`SO_PEERCRED`), and there are no TCP listeners.

## 5. Discover the settings

```sh
openrelay capabilities | jq .
```

This lists every key with its type, range and scope, plus the current `revision`. The keys:

| Key | Type | Range | Scope | What it does |
|---|---|---|---|---|
| `audio.sample_rate_hz` | integer | 48000 (fixed) | shared | sample rate |
| `audio.frame_duration_ms` | integer | 5 (fixed) | shared | frame size |
| `audio.gain_db` | number | −24 … 24, step 0.5 | shared | capture gain; the daemon scales PCM16 samples by 10^(dB/20) before publishing them on the virtual microphone |
| `audio.mute` | boolean | | shared | phone-side and PC-side mute: muted frames are not sent, and the PC side cuts the daemon's virtual microphone |
| `audio.jitter_target_ms` | integer | 5 … 40 | shared | Linux jitter buffer target |
| `audio.return_enabled` | boolean | | local | audio back to the phone; needs the `return-audio` Cargo feature (off by default) and `openrelay devices grant <fingerprint> return` |
| `ui.theme` | string | `system`, `light`, `dark` | local | app appearance |
| `ui.text_scale` | number | 1.0 … 2.0 | local | app text size |
| `device.local_volume` | number | 0.0 … 1.0 | local | device playback volume, not stream gain |
| `device.model` | string | ≤ 64 characters | readonly | reported model, informational |

`openrelay config set audio.mute true --rev N` mutes the daemon's virtual microphone from the PC; the phone's own mute stays independent. The audio path applies `audio.gain_db`, `audio.mute` and `audio.return_enabled`; every other shared key answers `unsupported` and leaves the revision untouched.

`audio.return_enabled` is a local key, not shared. Grant the phone the capability first with `openrelay devices grant <fingerprint> return` (it persists `return_audio` and requires a daemon restart), then `openrelay config set audio.return_enabled true --rev N`. The daemon must be built with the `return-audio` Cargo feature, which is off by default. A local key written over the remote channel answers `local_key`; a readonly key answers `denied`. Reads work for every key.

Defaults and ownership are in [config-schema.md](config-schema.md).

## 6. Read and write configuration

```sh
openrelay config get                     # full snapshot
openrelay config get audio.mute          # one key
```

A read answers with an `ack` that carries the `effective` value and the current `revision`:

```json
{"ok":true,"result":{"type":"ack","revision":0,
  "payload":{"applied":true,"effective":{"key":"audio.mute","value":false}}}}
```

Writes are **optimistic**. You pass the revision you last read, and if someone else changed the configuration in the meantime (the phone, for instance), the write is rejected instead of silently overwriting their change.

```sh
rev=$(openrelay config get audio.mute | jq -r .result.revision)
openrelay config set audio.mute true --rev "$rev"
```

On a conflict the CLI exits 1, prints the JSON error on stdout and a one-line reason on stderr. `current_revision` tells you what to read next:

```sh
$ openrelay config set audio.mute true --rev 7
{"error":{"code":"conflict","current_revision":0,"message":"revision mismatch","retryable":true},"ok":false,"request_id":"cli-1234-…"}
openrelay: the daemon returned an error      # stderr
```

`VALUE` is parsed as `true`/`false`, an integer, a decimal, or text, in that order. The key decides whether the value is valid. For example, `0.5` is not an integer.

## 7. Exit codes

| Code | Meaning |
|---|---|
| `0` | success (`ok: true`) |
| `1` | the daemon is unreachable, or it answered with an error (`conflict`, `unsupported`, `not_found`, `invalid`, `local_key`, `denied`, `busy`, …) |
| `2` | usage error: missing `--rev`, missing argument, unknown option, unset `XDG_RUNTIME_DIR` without `--socket` |

Running `openrelay` with no subcommand prints the usage text and exits 2; `openrelay --help` exits 0.

On an error, the JSON response still goes to stdout and a short reason goes to stderr, so scripts can always parse stdout. CLI messages are in English.

## 8. Use another socket

```sh
openrelay daemon --socket /tmp/openrelay-test.sock --no-quic &
openrelay status --socket /tmp/openrelay-test.sock
```

This is handy for testing a second instance, or when `XDG_RUNTIME_DIR` is not set (cron or a bare SSH session).

## 9. Everyday recipes

```sh
# Follow the logs of the service
journalctl --user -u openrelay.service -f

# Pair a phone from a desktop shortcut with a longer window
openrelay pair --ttl 300

# Check that the installed unit matches the one the binary ships with
diff <(openrelay service print-unit) ~/.config/systemd/user/openrelay.service && echo "unit up to date"
```

Pairing security is covered in [security/pairing.md](security/pairing.md).
