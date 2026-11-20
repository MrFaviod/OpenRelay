# Changelog

All notable changes to OpenRelay. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

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

[0.2.1]: https://github.com/MrFaviod/OpenRelay/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/MrFaviod/OpenRelay/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/MrFaviod/OpenRelay/releases/tag/v0.1.0
