# Changelog

All notable changes to OpenRelay. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [0.3.1] - 2026-11-26

### Fixed

- Android: the app no longer drops the session in a loop on phones whose microphone delivers bursts that are not a multiple of 240 samples (seen on a POCO X4 Pro with VoltageOS, 144-sample bursts). The bridge now buffers the capture and sends only complete PCM16/Opus frames, so the stream stays up instead of reconnecting until the daemon's per-device session quota rejects it.

## [0.3.0] - 2026-11-23

### Added

- New logo and app icon, with a themed (monochrome) icon on Android 13+.
- `openrelay daemon --tailscale` binds sessions and pairing to the PC's Tailscale address, so nothing listens on the LAN.
- Return audio from the PC to the phone: `devices grant`, `audio.return_enabled` and a switch in the app. The daemon's `return-audio` build feature stays off by default.
- `install.sh --update` and `install.sh --uninstall`.

### Changed

- The installer enables and starts the user service, and the CLI tells you how to start the daemon when it is not running.
- The capture gain (`audio.gain_db`) is applied to the audio, and `audio.mute` set from the PC mutes the virtual microphone.

## [0.2.2] - 2026-11-20

### Changed

- The app and the CLI are now in English.

## [0.2.1] - 2026-11-20

### Added

- Unpair a phone with `openrelay devices remove <fingerprint>`.
- `openrelay status` shows the sessions that are alive right now.

### Changed

- `install.sh` refuses to install when `libpipewire` or `libopus` is missing. Set `OPENRELAY_FORCE=1` to override.

### Fixed

- The app shows when the PC closes the session and reconnects by itself after a daemon restart.

## [0.2.0] - 2026-11-19

### Added

- Pairing with `openrelay pair`: QR code and link, with approval on the PC.
- Phone audio reaches the PipeWire source `openrelay-mic`.
- The daemon identity is created on first start.

## [0.1.0] - 2026-11-16

First preview.

[0.3.1]: https://github.com/MrFaviod/OpenRelay/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/MrFaviod/OpenRelay/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/MrFaviod/OpenRelay/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/MrFaviod/OpenRelay/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/MrFaviod/OpenRelay/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/MrFaviod/OpenRelay/releases/tag/v0.1.0
