# Linux→Android audio return

**Status: opt-in, off by default.** The sender lives behind the `return-audio` Cargo feature and needs three independent permissions (feature, configuration, ACL capability). The logic lives in `crates/daemon/src/return_audio.rs`, the monitor capture in `crates/audio-linux/src/monitor.rs`, and the gate in the daemon runtime.

## What it is

Return is the audio the Linux daemon sends to the Android device in the direction opposite to microphone capture. It reuses the `type=1` frame from [`protocol.md`](protocol.md) §8 unchanged: PCM16LE, 48 kHz, mono, 240 samples and 480 bytes of payload. There is no new profile, no new codec and no new control message.

## Opt-in and shutdown

It is **off by default** and requires two conditions at once:

1. **Feature on both ends.** Android advertises `audio.return-pcm16-48k-mono` in `capabilities` and the daemon only counts it as local **if `audio.return_enabled = true`**. If either side lacks it, there is no return.
2. **Daemon local key.** `audio.return_enabled = true` in the daemon configuration (see [`config-schema.md`](config-schema.md)); its default is `false`.

The effective local features come from the configuration, not from a constant: with the key off, the daemon does not advertise return to itself and does not open the stream. The daemon does not yet send a `capabilities` message of its own to the peer; when it does, it must advertise exactly that effective list.

If either condition is missing, the daemon **does not send a single return datagram**: there is no implicit negotiation and no silent fallback.

## One return stream at a time

The daemon uses **a single `MonitorSource`** (one capture stream of the PipeWire default sink) and serves return to **one session at a time**. If two peers advertise return, the first authorized session is served; the others receive no return until that one closes. Splitting a single capture ring across several peers would leave all of them with gaps, so it is not done. The `seq` sequence only advances when the frame send succeeded: a transport failure consumes no number and opens no false gap.

To turn it off:

- Set `audio.return_enabled = false` (or remove the key, which already defaults to `false`).
- The daemon is built with the Cargo feature `return-audio`, **off by default**; a binary without it cannot emit return even if the configuration asks for it.

## Echo warning

Before enabling return, the Android end must show an echo/feedback warning: audio playing on the device can re-enter through its microphone and feed back. Without that warning, return is not enabled. The warning is the Android client's responsibility; the contract does not replace it.

## Granting the capability

The transport requires the `return_audio` ACL capability for `send_audio` (in
addition to the two opt-in conditions above). Grant it to an already paired
phone from the CLI on the daemon machine:

```console
$ openrelay devices grant <fingerprint-hex> return
openrelay: return granted to <fingerprint-hex>
```

`devices` prints each phone's `return_audio=` flag, so the grant can be checked
without opening the identities file. The command writes `identities.json`
(0600) and the daemon reads it at startup: **restart the daemon** after
granting. Running the command twice is idempotent (`already granted`). The
command only grants `return`; `mic`, `control_read` and `control_write` are
left untouched.

## Turning it on from the app

The Android app has the switch under **Advanced → Return audio**. It is only
enabled while a session with the PC is open; without one, the app says so
instead of pretending. Turning it on first shows the echo warning described
above; only after the user confirms does the app acknowledge the warning,
enable the playback engine and attach it to the live session. Turning it off
stops the return pump and tells the PC with `setReturnAudio(false)`.

The app does not enable return by itself: the switch reflects the observed
pump state, not a stored preference.

## Out of scope

- **Android app capture.** Return does not capture or play audio from other apps on the device; it only transports the stream the Linux daemon produces.
- **Mixing with the microphone.** Return is an independent stream, with its own `epoch` and `seq`. It is never mixed into the same datagram or the same epoch as the Android→Linux audio.
- **Any format other than PCM16LE 48 kHz mono.** No resampling or other profile is negotiated in v1.
