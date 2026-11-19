<p align="center">
  <img src="docs/assets/banner.jpg" alt="OpenRelay: your Android phone's microphone, streamed to Linux" width="100%">
</p>

<h1 align="center">OpenRelay</h1>

<p align="center">
  <b>Turn your Android phone into a low-latency microphone for your Linux PC, over your own LAN.</b><br>
</p>

<p align="center">
  <a href="https://github.com/MrFaviod/OpenRelay/releases/latest"><img alt="Release" src="https://img.shields.io/github/v/release/MrFaviod/OpenRelay?include_prereleases&style=flat-square&color=14b8a6"></a>
  <a href="https://github.com/MrFaviod/OpenRelay/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/MrFaviod/OpenRelay/ci.yml?branch=main&style=flat-square&label=CI"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-8b5cf6?style=flat-square"></a>
  <img alt="Linux x86_64 | aarch64" src="https://img.shields.io/badge/linux-x86__64%20%7C%20aarch64-0f172a?style=flat-square&logo=linux&logoColor=white">
  <img alt="Android 9+" src="https://img.shields.io/badge/android-9%2B-0f172a?style=flat-square&logo=android&logoColor=3ddc84">
  <img alt="Rust 1.80" src="https://img.shields.io/badge/rust-1.80-0f172a?style=flat-square&logo=rust">
</p>

<p align="center">
  <a href="#install">Install</a> ·
  <a href="docs/cli-quickstart.md">CLI quick guide</a> ·
  <a href="INSTALL.md">Install guide</a> ·
  <a href="docs/protocol.md">Protocol</a> ·
  <a href="docs/security/threat-model.md">Security</a>
</p>

---

## The idea

You have a good microphone in your pocket and a Linux box with a bad one, or none at all. OpenRelay connects the two on your local network, and the phone shows up as a normal input device (`openrelay-mic`) that any app can record from.

```
 ┌──────────────┐ QUIC + mutual TLS, LAN only ┌──────────────────────────┐
 │ Android app │ ══════════════════════════════════► │ openrelay daemon │
 │ mic capture │ audio: unreliable datagrams │ PipeWire "openrelay-mic"│
 │ control UI │ ◄─────────────────────────────────── │ `openrelay` CLI │
 └──────────────┘ control: one reliable stream └──────────────────────────┘
```

- **Latency first.** Audio rides QUIC datagrams. A late frame is dropped, never retransmitted, and a small jitter buffer (10 ms by default) smooths the stream on the Linux side.
- **Secure by construction.** Pairing uses a one-time QR token, an SPKI pin and explicit approval on the PC. Every session after that is mutual TLS. There is no cleartext fallback and no `verify=false`.
- **Local only.** The daemon accepts inbound connections from paired devices and nothing else. No HTTP API, no remote shell, no outbound connections.
- **Small.** A single Rust binary, about 3.5 MB (2.8 MB stripped), for the daemon and CLI, an optional `systemd --user` unit, and a native Kotlin app. Linux has no GUI.
- **Shared control.** Phone and CLI speak one configuration contract: revisions, conflict detection, idempotent retries. v0.1.0 has no hardware applicator yet, so a write is refused with `unsupported` instead of faked.

## Screenshots

<p align="center">
  <img src="docs/assets/app-home.png" alt="Idle home screen" width="30%">
  &nbsp;
  <img src="docs/assets/app-capture.png" alt="Capture running with mute control" width="30%">
  &nbsp;
  <img src="docs/assets/app-advanced.png" alt="Advanced section with requested and effective gain" width="30%">
</p>
<p align="center"><sub>Idle · capturing, with mute and input level · advanced settings, showing requested vs. effective gain</sub></p>

## Status: early preview

> [!IMPORTANT]
> **v0.1.0 is a developer preview, not a finished product.** The building blocks are implemented and tested, but the phone app **cannot pair with a PC from its UI yet**, so you can't stream your voice end to end from a fresh install. If you want a daily-driver mic today, this release isn't it yet. If you want to test, review or hack on it, welcome.

| Piece | State |
|---|---|
| Control protocol v1: revisions, conflicts, idempotency | ✅ implemented, JSON-Schema checked |
| QUIC transport, mTLS sessions, rate limiting | ✅ implemented, regression and fuzz tested |
| Pairing core: one-time token, SPKI pin, local approval, ACL | ✅ in the daemon · ⏳ not exposed in the CLI or app yet |
| PipeWire virtual source `openrelay-mic` | ✅ loopback verified at 100/100 frames on a private PipeWire |
| PulseAudio and ALSA backends | ✅ implemented · ⏳ untested on real hardware |
| Opus codec, with PCM16 fallback | ✅ implemented |
| Android app: capture, foreground service, mute, gain request | ✅ capture and controls work locally · ⏳ FGS lifecycle verified on API 28/34/37 emulators, no audio input |
| Noise suppression (SpeexDSP) | 🧪 optional feature, off by default |
| Audio back to the phone | 🧪 opt-in, off by default |
| Real phones, real LANs, other distros | ⏳ [needs testers](https://github.com/MrFaviod/OpenRelay/issues) |

The [compatibility matrix](docs/compatibility-matrix.md) lists exactly what has been verified, and where.

## Install

**Linux** (x86_64 or aarch64; glibc 2.34+ and PipeWire 0.3.65 or newer). No root needed. It installs into `~.local/bin`.

```sh
curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | sh
```

The script detects your architecture, downloads the release tarball, **verifies it against `SHA256SUMS`** before installing anything, and puts the optional user unit in `~.config/systemd/user/` without enabling it. Pin a version with `OPENRELAY_VERSION=v0.1.0`, or uninstall with `OPENRELAY_UNINSTALL=1`.

**Android** 9 or newer: download `openrelay-0.1.0-android-debug.apk` from the [latest release](https://github.com/MrFaviod/OpenRelay/releases/latest) and install it. It is a debug-signed preview build.

Rather read before you pipe? The [install guide](INSTALL.md) covers manual installation, per-distro libraries and building from source.

## 30-second tour

```sh
openrelay daemon & # start it in the background for this tour (or use the unit)
openrelay status # alive? prints JSON: version, uptime, sessions, QUIC address
openrelay capabilities # every key with type, range and the current revision
openrelay config get # full configuration snapshot
openrelay config get audio.mute # read one key
```

Writes look like `openrelay config set audio.mute true --rev 0`. The revisioned write path is implemented, but v0.1.0 has no hardware applicator: the daemon answers `unsupported` and leaves the revision untouched rather than claiming a change it did not make.

As a user service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now openrelay.service
journalctl --user -u openrelay.service -f
```

Everything else is in the **[CLI quick guide](docs/cli-quickstart.md)**.

## Repository layout

```text
crates
  audio-core/ frames, PCM16, jitter buffer, clock drift, latency budget
  audio-linux/ PipeWire source, PulseAudio and ALSA backends
  codec/ Opus wrapper with PCM16 fallback
  dsp/ optional noise suppression (SpeexDSP)
  proto/ strict control and wire format (openrelay.control/v1)
  transport/ QUIC (quinn), mTLS identity, enrollment, rate limits
  daemon/ runtime, sessions, ACL, pairing, return audio
  cli/ the `openrelay` binary
android/ Kotlin app, Rust JNI bridge, native capture in C
contracts/ JSON Schema 2020-12 for control messages and errors
packaging/ systemd --user unit and tarball notes
fuzz/ cargo-fuzz targets for the untrusted parsers
docs/ protocol, security, ADRs, config keys, latency budget
```

## Documentation

| | |
|---|---|
| 🚀 [Install guide](INSTALL.md) | curl, manual tarball, source build, Android |
| ⌨️ [CLI quick guide](docs/cli-quickstart.md) | everyday commands, config keys, exit codes |
| 📡 [Protocol v1](docs/protocol.md) | control stream, audio datagrams, limits |
| 🔐 [Threat model](docs/security/threat-model.md) · [Pairing](docs/security/pairing.md) · [Hardening](docs/security/hardening.md) | what is defended, and how |
| ⚙️ [Configuration keys](docs/config-schema.md) | every key with range, default and owner |
| 🧭 [Architecture decisions](docs/adr/) | transport, identity, noise suppression, enrollment |
| ⏱️ [Latency budget](docs/latency-budget.md) · [Measuring it](docs/measurement-method.md) | per-stage targets and the measurement method |

The README, install guide and CLI quick guide are in English. Most deep-dive documents are in Spanish.

## Building from source

```sh
# Ubuntu 24.04+ / Debian 12+
sudo apt install build-essential pkg-config clang libclang-dev cmake \
  libpipewire-0.3-dev libspa-0.2-dev libpulse-dev libopus-dev

git clone https://github.com/MrFaviod/OpenRelay && cd OpenRelay
cargo build --release --locked -p openrelay-cli # -> target/release/openrelay
cargo test --locked --workspace
```

The toolchain is pinned to Rust 1.80.1 in `rust-toolchain.toml`. The Android build is covered in [INSTALL.md](INSTALL.md#android-from-source).

## Contributing

Reports from real hardware are the most useful thing right now. Include your distro, audio server (`pactl info | head -3`), phone model, Android version, and the output of `openrelay status`.

Before you open a PR:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
```

## License

[MIT](LICENSE) © OpenRelay contributors. Dependencies keep their own permissive licenses. `libpulse` is linked dynamically. See [licensing](docs/licensing.md) and the [dependency ledger](docs/dependency-ledger.md).
