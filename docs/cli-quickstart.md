# `openrelay` CLI: quick guide

One binary does two jobs. It is the **daemon** (`openrelay daemon`) and the **client** that talks to it over a local Unix socket. Every command that talks to the daemon prints JSON on stdout, so it pipes cleanly into `jq`.

```text
openrelay daemon [--socket PATH] run the daemon in the foreground
                        [--quic ADDR --identity-der PATH --identity-key PATH
                         --client-ca PATH --identities PATH --control-grants PATH]
openrelay status [--socket PATH] daemon state
openrelay capabilities [--socket PATH] keys, ranges, current revision
openrelay config get [KEY] [--socket PATH] read one key, or all of them
openrelay config set KEY VALUE --rev N [--socket PATH] write one key
openrelay service print-unit print the systemd --user unit
openrelay --help | openrelay <command> --help
```

## 1. Start the daemon

```sh
openrelay daemon
```

It stays in the foreground until Ctrl-C or SIGTERM, and never forks; there is no `--foreground` flag. A socket left behind by a killed process is removed on the next start, while a live one is an error. To run it in the background, use the user unit:

```sh
systemctl --user enable --now openrelay.service
```

The control socket lives at `$XDG_RUNTIME_DIR/openrelay/control.sock`. Its directory is mode 0700 and the socket is 0600. It only accepts clients with the **same UID** (`SO_PEERCRED`), and it never listens on TCP.

## 2. Check it

```sh
openrelay status
openrelay status | jq.
```

## 3. Discover the settings

```sh
openrelay capabilities | jq.
```

This lists every key with its type, range and scope, plus the current `revision`. The keys:

| Key | Type | Range | Scope | What it does |
|---|---|---|---|---|
| `audio.sample_rate_hz` | integer | 48000 (fixed) | shared | sample rate |
| `audio.frame_duration_ms` | integer | 5 (fixed) | shared | frame size |
| `audio.gain_db` | number | −24 … 24, step 0.5 | shared | capture gain, applied on the phone |
| `audio.mute` | boolean | | shared | mute the stream |
| `audio.jitter_target_ms` | integer | 5 … 40 | shared | Linux jitter buffer target |
| `audio.return_enabled` | boolean | | local | audio back to the phone (opt-in) |
| `ui.theme` | string | `system`, `light`, `dark` | local | app appearance |
| `ui.text_scale` | number | 1.0 … 2.0 | local | app text size |
| `device.local_volume` | number | 0.0 … 1.0 | local | device playback volume, not stream gain |
| `device.model` | string | ≤ 64 characters | readonly | reported model, informational |

Only `shared` keys accept `config set`. A `local` key answers `local_key` and a `readonly` key answers `denied`; the daemon and the app own those. In v0.1.0 the daemon also has no hardware applicator, so even a valid write to a `shared` key answers `unsupported` and leaves the revision untouched. Reads work for every key.

Defaults and ownership are in [config-schema.md](config-schema.md).

## 4. Read and write configuration

```sh
openrelay config get # full snapshot
openrelay config get audio.mute # one key
```

A read answers with an `ack` that carries the `effective` value and the current `revision`:

```json
{"ok":true,"result":{"type":"ack","revision":0
  "payload":{"applied":true,"effective":{"key":"audio.mute","value":false}}}}
```

Writes are **optimistic**. You pass the revision you last read, and if someone else changed the configuration in the meantime (the phone, for instance), the write is rejected instead of silently overwriting their change.

```sh
rev=$(openrelay config get audio.mute | jq -r.result.revision)
openrelay config set audio.mute true --rev "$rev"
```

On a conflict the CLI exits 1, prints the JSON error on stdout and a one-line reason on stderr. `current_revision` tells you what to read next:

```sh
$ openrelay config set audio.mute true --rev 7
{"error":{"code":"conflict","current_revision":0,"message":"revision mismatch","retryable":true},"ok":false,"request_id":"cli-1234-…"}
openrelay: el daemon devolvió error # stderr
```

`VALUE` is parsed as `true`/`false`, an integer, a decimal, or text, in that order. The key decides whether the value is valid. For example, `0.5` is not an integer.

## 5. Exit codes

| Code | Meaning |
|---|---|
| `0` | success (`ok: true`) |
| `1` | the daemon is unreachable, or it answered with an error (`conflict`, `unsupported`, `not_found`, `invalid`, `local_key`, `denied`, …) |
| `2` | usage error: missing `--rev`, missing argument, unknown option, unset `XDG_RUNTIME_DIR` without `--socket` |

Running `openrelay` with no subcommand prints the usage text and exits 2; `openrelay --help` exits 0.

On an error, the JSON response still goes to stdout and a short reason goes to stderr, so scripts can always parse stdout. CLI messages are currently in Spanish.

## 6. Use another socket

```sh
openrelay daemon --socket /tmp/openrelay-test.sock &
openrelay status --socket /tmp/openrelay-test.sock
```

This is handy for testing a second instance, or when `XDG_RUNTIME_DIR` is not set (cron or a bare SSH session).

## 7. Accept phones on the network (advanced)

The session endpoint is **off by default**. To open it, give the daemon a UDP address and the five provisioned identity files:

```sh
openrelay daemon --quic 0.0.0.0:47000 \
  --identity-der server.der --identity-key server.key.der \
  --client-ca client-ca.der --identities identities.json \
  --control-grants grants.json
```

The daemon never generates keys or certificates on its own. In v0.1.0 the in-app QR pairing flow is not exposed yet, so this mode is meant for development and testing. See [security/pairing.md](security/pairing.md) and [transport-api.md](transport-api.md).

## 8. Everyday recipes

```sh
# Follow the logs of the service
journalctl --user -u openrelay.service -f

# Mute and unmute from a hotkey script
# (v0.1.0 refuses the write with `unsupported`; the command shape is final)
rev=$(openrelay config get | jq -r '.result.revision')
openrelay config set audio.mute true --rev "$rev"

# Check that the installed unit matches the one the binary ships with
diff <(openrelay service print-unit) ~.config/systemd/user/openrelay.service && echo "unit up to date"
```

The full socket protocol (one JSON line per request) is documented in [cli.md](cli.md).
