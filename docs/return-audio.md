# Linux→Android audio return

**Status: not available in 0.2.2.** The code exists in the daemon behind the `return-audio` feature, off by default. The logic lives in `crates/daemon/src/return_audio.rs`, the monitor capture in `crates/audio-linux/src/monitor.rs`, and the gate in the daemon runtime.

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

## Not available in 0.2.2

The transport requires the `return_audio` ACL capability for `send_audio` (in
addition to the two opt-in conditions). Although `pairing::approve` knows how to
persist the `return_audio` flag in the identities file and `pairing::load` reads
it back, no product path grants it: the app does not advertise the feature and
the CLI exposes no command to grant it. Granting it requires calling
`pairing::approve` with `Capabilities::with_return_audio()` from an approval
flow that does not exist yet.

## Out of scope

- **Android app capture.** Return does not capture or play audio from other apps on the device; it only transports the stream the Linux daemon produces.
- **Mixing with the microphone.** Return is an independent stream, with its own `epoch` and `seq`. It is never mixed into the same datagram or the same epoch as the Android→Linux audio.
- **Any format other than PCM16LE 48 kHz mono.** No resampling or other profile is negotiated in v1.
