# Changelog

All notable changes to OpenRelay. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [0.2.1] - 2026-11-20

Documentation now matches what was executed. No protocol change.

### Added

- `openrelay devices remove <fingerprint>`: revokes an approved phone, removes it from `identities.json`, drops it from the live ACL and closes its session.
- `openrelay status` reports sessions alive right now instead of a cumulative counter.
- `scripts/e2e/run.sh`: end-to-end harness on `ubuntu:24.04` with PipeWire, WirePlumber and pipewire-pulse, `install.sh` from a local tarball, and a synthetic phone; 28 checks ending in `E2E: PASS`.
- Docs: the emulator real-app run with measured RMS/frequency, and an explicit list of what is still unproven.

### Changed

- Android build `versionName 0.2.1`, `versionCode 3`.
- `install.sh` checks the downloaded binary for `libpipewire` and `libopus` **before** installing and aborts with the package names unless `OPENRELAY_FORCE=1`.
- Documentation corrections: `audio.gain_db` is stored but not applied; `audio.mute` is phone-side only; Opus is not negotiated by the app (PCM16 in practice); latency figures are targets, not measurements; return audio is not available; every `config set` answers `unsupported`; the systemd unit writes both `$XDG_RUNTIME_DIR` and `~.config`.

### Fixed

- The Android app reports a closed or rejected session instead of showing a dead link, and reconnects on its own after a daemon restart.

## [0.2.0] - 2026-11-19

Pairing and streaming work end to end.

### Added

- `openrelay pair`: opens a 120 s enrollment window (adjustable with `--ttl`), prints an `openrelay://pair` URI and a terminal QR, shows the device fingerprint and asks for local approval.
- `openrelay devices`: lists approved identities and their capabilities from `identities.json`.
- Android pairing UI: **Emparejar con PC** (QR scan or pasted URI), **Esperando aprobación en la PC…**, **Emparejado con** the host, and **Olvidar PC**.
- Microphone streaming from the app to the daemon, decoded into the PipeWire source `openrelay-mic`; the app shows **Enviando a** the host while capturing.
- Daemon identity auto-provisioning in `${XDG_CONFIG_HOME:-~.config}/openrelay/`: server certificate and key, client CA and key, `identities.json`, `grants.txt`, all mode 0600.
- QUIC session endpoint enabled by default on `0.0.0.0:47000`; `--quic ADDR` moves it and `--no-quic` disables it.
- Daemon operations `pair.open`, `pair.pending`, `pair.approve`, `pair.reject`, `pair.close`, `audio.stats`, and `status` fields `pipewire` and `pair`.
- Android build `versionName 0.2.0`, `versionCode 2`.

### Fixed

- The pairing window no longer leaks when `openrelay pair` exits early. Ctrl-C, EOF on stdin, a rejected prompt, a QR failure or an error during polling closes the window (the CLI calls `pair.close`), and the daemon also reaps a window whose CLI process died via the `owner_pid` sent in `pair.open`.

### Changed

- Android enrollment is pin-only, and the session certificate is validated by its fixed TLS name `openrelay` rather than the QR address.

## [0.1.0] - 2026-11-16

First developer preview.

### Added

- Control protocol v1 over a same-UID Unix socket: revisions, conflict detection, idempotent retries.
- QUIC transport with mutual TLS sessions, ACLs, rate limiting and fuzz targets.
- Pairing core: one-time token, SPKI pin, local approval, persisted ACL.
- PipeWire virtual source `openrelay-mic`, plus PulseAudio/ALSA backends.
- Opus codec with PCM16 fallback, jitter buffer and latency budget.
- Android app with microphone capture, foreground service, mute and gain request.
- Optional SpeexDSP noise suppression and opt-in audio return to the phone.
- `install.sh`, systemd `--user` unit, release tarballs.

[0.2.1]: https://github.com/MrFaviod/OpenRelay/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/MrFaviod/OpenRelay/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/MrFaviod/OpenRelay/releases/tag/v0.1.0
