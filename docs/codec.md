# Opus codec

**Status: implemented at crate level, not integrated into the session.** The
Android app does not negotiate Opus yet; the real stream is PCM16, and the
README says so. The fixed PCM16 path remains
the default contract and is unchanged. This document describes
`openrelay-codec`, its profiles and its limits; it does not claim quality by
listening or measured end-to-end latency.

## Profiles

All profiles use `opus::Application::LowDelay`
(`OPUS_APPLICATION_RESTRICTED_LOWDELAY`): CELT only, mono, 48 kHz, PCM16
as input and output.

| Profile | Block | Samples | Packet at 64 kbps | Packet at 96 kbps | Algorithmic latency (block + lookahead) |
|---|---:|---:|---:|---:|---:|
| `LowDelay2p5Ms` | 2.5 ms | 120 | ~20 B | ~30 B | 5.0 ms |
| `LowDelay5Ms` | 5 ms | 240 | ~40 B | ~60 B | 7.5 ms |
| `LowDelay10Ms` | 10 ms | 480 | ~80 B | ~120 B | 12.5 ms |

- Accepted bitrate: **64–96 kbps mono**; outside that range
  `OpusEncoder::new` returns `InvalidBitrate`.
- The bitrate is a target: libopus uses VBR by default, so the real size
  varies per frame. The hard cap is `MAX_PACKET_LEN` = 1200 − 28 = **1172 bytes**
  (transport datagram minus the v1 frame header).
- Algorithmic latency is block + lookahead; **it is not end-to-end latency**.
  The lookahead in restricted-lowdelay mode is 2.5 ms (120 samples)
  and is covered by `lookahead_is_2_5_ms`.

## Comparison with fixed PCM16

PCM16 mono 48 kHz takes 2 bytes per sample: **480 bytes per 5 ms frame =
768 kbps** of payload, plus 28 bytes of header per datagram. Opus does not change
the frame contract: if adopted, its packet replaces the PCM16 payload inside
the same datagram budget.

| Path | 5 ms payload | Payload bitrate | Datagram with header |
|---|---:|---:|---:|
| Fixed PCM16 | 480 B | 768 kbps | 508 B |
| Opus 5 ms at 64 kbps | ~40 B | 64 kbps | ~68 B |
| Opus 5 ms at 96 kbps | ~60 B | 96 kbps | ~88 B |

The bandwidth reduction is roughly 8–12× at 5 ms. No
transparency or "better quality" is claimed: Opus is a lossy codec and quality
comparison requires listening, which this deliverable does not include.

## Packet loss: PLC, not FEC

- `OpusDecoder::decode(None)` (or `Some(&[])`) decodes without a packet and libopus
  applies **PLC** with the last known packet duration. The output is always
  exactly one frame of the profile.
- PLC depends on decoder state: the stream has to be decoded in
  order and not restarted between losses. A long loss degrades the concealed
  signal; PLC does not recover the lost content.
- **In-band FEC: not available and not promised.** In libopus, FEC belongs to
  SILK (and hybrid modes); `OPUS_APPLICATION_RESTRICTED_LOWDELAY` uses CELT only.
  This crate never calls `set_inband_fec` and always decodes with
  `fec = false`. The receiver must not be configured expecting FEC.

## 44.1 kHz input

libopus only accepts 8, 12, 16, 24 or 48 kHz; 44.1 kHz is not a valid input
rate. A 44.1 kHz source must be resampled to 48 kHz **before** the
encoder:

- `openrelay_audio_core::LinearResampler` is a drift corrector: its ratio is
  clamped to `[0.98, 1.02]`, so it cannot serve 44.1 → 48 (ratio 1.088).
- The 44.1 → 48 resampling belongs to the earlier capture/adaptation stage,
  outside this crate. `OpusEncoder::encode` rejects any buffer that does not
  hold exactly the profile's samples at 48 kHz (`InvalidPcmLength`).

## Fallback and rollback to PCM16

The codec is **optional**:

- The v1 frame and the PCM16 pipeline do not depend on `openrelay-codec`; the crate does not touch
  `openrelay-audio-core`, the transport or the session.
- Profile selection and the announcement that Opus is in use belong to the
  session/negotiation layer (outside this crate's scope). In v1 there is no
  autodetection: the receiver must know whether the payload is PCM16 or Opus.
- **Rollback**: if the profile is not negotiated, the codec fails or the quality is not
  acceptable, the sender goes back to emitting fixed PCM16. The receiver keeps
  the PCM16 path as a fallback; data already sent does not have to be converted.
- A new `OpusEncoder`/`OpusDecoder` per session and per direction; they are not
  `Sync`, so each state is used from one thread at a time.

## API

- `CodecProfile::{LowDelay2p5Ms, LowDelay5Ms, LowDelay10Ms}` with
  `samples_per_frame()`, `frame_duration_us()`, `from_frame_samples()` and `ALL`.
- `OpusEncoder::new(profile, bitrate_bps)`, `encode(&[i16]) -> Vec<u8>`,
  `lookahead_samples()`, `reset()`.
- `OpusDecoder::new(profile)`, `decode(Option<&[u8]>) -> Vec<i16>`, `reset()`.
- Errors in `CodecError` (no `thiserror`): invalid sizes and bitrates,
  wrong PCM lengths, packet too large and libopus errors.

## Dependencies and licenses

| Component | Version | License | Note |
|---|---|---|---|
| `opus` (crate) | =0.3.0 | MIT OR Apache-2.0 | safe bindings |
| `audiopus_sys` | 0.2.2 | ISC | FFI and libopus build |
| `libopus` (bundled) | per `audiopus_sys` | BSD-3-Clause | built with CMake |

The build requires CMake and a C compiler. Our crate keeps
`unsafe_code = "forbid"`; `unsafe` lives only in the FFI dependencies.

## Verification

```sh
cargo test --locked -p openrelay-codec --test codec_profiles
cargo clippy -p openrelay-codec -- -D warnings
cargo fmt -p openrelay-codec -- --check
```

`codec_profiles` covers: roundtrip at 2.5/5/10 ms and bitrates 64/80/96 kbps,
rejection of invalid sizes and bitrates, PLC with the correct length, a packet
within 1200 − 28 bytes, SNR/energy against a sine and a 2.5 ms lookahead.
Passing these tests does not prove perceptual quality, E2E latency or
network interoperability.
