# arl — an MIT alternative to the reference app

**Working name: `arl`.** Not a brand, not an approved product name, and not a distribution channel. The name and distribution decision (D-11) is still open.


## What it is

Free software under the MIT license, meant as an alternative to the reference app for **Linux and Android**:

- Linux in Rust, a small CLI, no desktop GUI. It can run in the foreground or as an optional `systemd --user` unit.
- A native Android app as the control panel and the microphone source.
- The main audio path: **Android microphone → Linux**, over the LAN, latency ahead of quality.
- Optional noise reduction, built in, in a later phase. No library is chosen yet.
- Control is **bidirectional**: the CLI and the panel share configuration and state. Audio back from Linux to Android is a separate phase, opt-in and off by default.

## What it is not

- **Not** bidirectional audio on the main path. "Bidirectional" here means control.
- **Not** a way to capture audio from other Android apps. That stays out unless it is explicitly approved and planned on its own.
- **Not** a promise that every distribution, driver, app or device works. Compatibility is a matrix of cells that are verified, limited, or still open.
- **No** cloud, no account, no telemetry, no external service.
- **No** admin API over the LAN, no remote shell, no control over HTTP.
- **No** `verify=false` or anything like it, and no fallback to cleartext UDP.

## Decisions that shape the repository

- **(x86) — settled by the maintainer on 2026-11-13:** targets are **x86-64** and **ARM64**, with **no 32-bit support**. The proposed GNU targets are `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.
- **(Android) — frozen at:** created no Gradle project and no Android app. minSdk 28 is a proposal; target and compile SDK are fixed at and re-checked at.
- **(baseline) — no physical evidence:** no latency, CPU or memory number in this repository is a measured result. The budget in [docs/latency-budget.md](docs/latency-budget.md) is a proposal.
- **D-11 (name and distribution) — open:** `arl` is provisional, and no release channel is decided.

## Layout

```text
README.md this file
INDEX.md the single root index of the documentation
AGENTS.md working rules for the repository
LICENSE MIT
Cargo.toml Rust workspace (resolver 2, members crates/*)
Cargo.lock lockfile with checksums
rust-toolchain.toml toolchain pinned to Rust 1.80.1
deny.toml proposed cargo-deny policy (not run)
crates/audio-core/ frame v1, PCM16, jitter, metrics, drift, budget
crates/proto/ strict control and wire format (pinned serde, serde_json)
docs/plan/ plan and coverage (historical snapshots) plus the execution record
docs/security/ threat model and pairing flow
docs/protocol.md control and audio contract
docs/config-schema.md requested versus effective configuration, and local keys
docs/dependency-ledger.md effective deps, lock-only deps, candidates and exclusions
docs/latency-budget.md per-stage and resource budget (a proposal)
contracts/ JSON Schema 2020-12 and the error catalog
scripts/check-doc-links.sh local link checker (bash, no dependencies)
scripts/ci-doc.sh per-phase docs and core gate
tools/verify.py multi-mode docs verifier (stdlib)
```

## Check the current phase

```sh
bash scripts/ci-doc.sh --phase f3
```

The gate checks links, schemas, contracts, the dependency ledger against Cargo's real graph, the audio budget, the fail-closed file inventory, `cargo fmt --check`, `clippy` with warnings as errors, `cargo doc` with `RUSTDOCFLAGS=-Dwarnings`, and the tests. The drift simulation of one hour is marked `#[ignore]`; it is not a benchmark and not a hardware measurement.

`--phase f1` and `--phase f2` are historical gates. On this tree they fail on the file inventory, and that failure is the expected one. The GitHub Actions workflow **has not run** on a runner: the evidence is local.

## Where to go next

- [Root index](INDEX.md)
- [Project status](docs/status.md)
- [/ execution record](docs/plan/execution-f2-f3.md)
- [Threat model](docs/security/threat-model.md)
- [Pairing](docs/security/pairing.md)
- [Protocol v1](docs/protocol.md)
- [Configuration](docs/config-schema.md)
- [Latency budget](docs/latency-budget.md)
- [Errors](contracts/errors.md)

## License

Own code and docs are **MIT**; see [LICENSE](LICENSE). Dependencies keep their own licenses and are recorded separately in the [ledger](docs/dependency-ledger.md). No GPL component is copied or linked by accident; `libpulse`, if used, is linked dynamically and its LGPL obligations are stated.
