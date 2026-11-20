# Installing OpenRelay

OpenRelay has two halves: the **Linux daemon and CLI** (`openrelay`) and the **Android app**. Install both.

- [Requirements](#requirements)
- [Linux: one-line install](#linux-one-line-install)
- [Linux: manual install from a release](#linux-manual-install-from-a-release)
- [Run it as a user service](#run-it-as-a-user-service)
- [Firewall](#firewall)
- [Android](#android)
- [Pair a phone](#pair-a-phone)
- [Build from source](#build-from-source)
- [Uninstall](#uninstall)
- [Troubleshooting](#troubleshooting)

## Requirements

| | |
|---|---|
| CPU | x86_64 or aarch64 (ARM64). No 32-bit builds. |
| Linux | glibc 2.34 or newer, with **PipeWire 0.3.65 or newer** (Ubuntu 24.04+, Debian 12+, Fedora 39+, Arch, …) |
| Libraries | `libpipewire-0.3`, `libopus` |
| Android | 9 (API 28) or newer |
| Network | Phone and PC on the same LAN. UDP must be allowed between them. |

Runtime libraries per distro:

```sh
# Ubuntu 24.04+
sudo apt install libpipewire-0.3-0t64 libopus0
# Debian 12 (pre-t64 package name)
sudo apt install libpipewire-0.3-0 libopus0
# Fedora
sudo dnf install pipewire-libs opus
# Arch / Manjaro
sudo pacman -S libpipewire opus
```

On any desktop that already runs PipeWire, these are almost certainly installed.

## Linux: one-line install

```sh
curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | sh
```

What it does, step by step:

1. Detects `x86_64` or `aarch64`, and refuses anything else.
2. Resolves the latest release, or the one in `OPENRELAY_VERSION`.
3. Downloads `openrelay-<version>-<target>.tar.gz` and `SHA256SUMS`.
4. **Verifies the checksum.** On a mismatch it stops and installs nothing.
5. Checks the downloaded binary for missing shared libraries (`libpipewire`, `libopus`) **before installing** and aborts with the package names unless `OPENRELAY_FORCE=1` is set.
6. Installs `openrelay` into `~.local/bin` (mode 0755) and `openrelay.service` into `~.config/systemd/user/`. It does **not** enable the unit, and it warns if `~.local/bin` is not in your `PATH`.

It never uses `sudo`, and by default it writes only under your home directory.

| Variable | Effect |
|---|---|
| `OPENRELAY_VERSION=v0.2.1` | install a specific release |
| `OPENRELAY_BIN_DIR=/some/dir` | install the binary somewhere else |
| `OPENRELAY_NO_UNIT=1` | skip the systemd unit |
| `OPENRELAY_FORCE=1` | install even when `libpipewire` or `libopus` is missing |
| `OPENRELAY_UNINSTALL=1` | remove the binary and the unit |

Example:

```sh
curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | OPENRELAY_VERSION=v0.2.1 sh
```

## Linux: manual install from a release

```sh
VERSION=0.2.1
TARGET=x86_64-unknown-linux-gnu # or aarch64-unknown-linux-gnu
BASE=https://github.com/MrFaviod/OpenRelay/releases/download/v$VERSION

curl -fLO "$BASE/openrelay-$VERSION-$TARGET.tar.gz"
curl -fLO "$BASE/SHA256SUMS"
grep " openrelay-$VERSION-$TARGET.tar.gz\$" SHA256SUMS | sha256sum -c -

tar -xzf "openrelay-$VERSION-$TARGET.tar.gz"
install -Dm0755 "openrelay-$VERSION/openrelay" ~.local/bin/openrelay
install -Dm0644 "openrelay-$VERSION/openrelay.service" ~.config/systemd/user/openrelay.service
```

The release tarball contains four files:

```text
openrelay-0.2.1
  openrelay the daemon and CLI
  openrelay.service optional systemd --user unit
  README.md packaging notes
  LICENSE MIT
```

Check it runs:

```sh
openrelay --help
openrelay service print-unit | head -3
```

## Run it as a user service

The unit is optional. `openrelay daemon` runs fine in a terminal too.

```sh
systemctl --user daemon-reload
systemctl --user enable --now openrelay.service
systemctl --user status openrelay.service
openrelay status
```

Logs go to the user journal:

```sh
journalctl --user -u openrelay.service -f
```

The unit runs with your own UID and a hardened sandbox (`ProtectSystem=strict`, `NoNewPrivileges`, and so on). It can only write to `$XDG_RUNTIME_DIR` and `~.config` (`ReadWritePaths=%t %h.config`): the control socket lives in the former and the daemon identity in `~.config/openrelay`. To keep it running after you log out, `loginctl enable-linger $USER` is needed, and that may require an administrator. Details are in [docs/systemd.md](docs/systemd.md).

## Firewall

The daemon needs inbound UDP from the phone's LAN:

| Port | When |
|---|---|
| **47000/udp** | always while the daemon runs: QUIC session endpoint |
| **47001/udp** | only while a pairing window is open |

```sh
# ufw (Ubuntu, Debian, …)
sudo ufw allow 47000:47001/udp

# firewalld (Fedora, …)
sudo firewall-cmd --permanent --add-port=47000-47001/udp
sudo firewall-cmd --reload

# nftables: allow udp dport 47000-47001 in your input chain
```

`--quic ADDR` changes the session bind address and port; `--no-quic` disables the session endpoint entirely. The enrollment port is fixed at 47001 and only exists during `openrelay pair`.

## Android

1. Download `openrelay-<version>-android-debug.apk` from the [releases page](https://github.com/MrFaviod/OpenRelay/releases).
2. Open it on the phone and allow installation from that source when Android asks.
3. Launch **OpenRelay** and grant the microphone permission. Notification permission is optional, but it lets you stop and mute from the shade.

> [!NOTE]
> The v0.2.1 APK is **debug-signed** and still a preview. Enable *install from unknown sources* for the browser or file manager you download it with, then open the APK. It pairs with the PC — paste the `openrelay://pair` link or scan the QR — and streams the microphone to the PC as PCM16.

## Pair a phone

1. Check that the daemon answers (`openrelay status`) and that UDP **47000-47001** is open in the host firewall ([Firewall](#firewall)).
2. On the PC run `openrelay pair`. `--ttl N` sets the window in seconds (1-300, default 120); `--advertise IP` overrides the LAN address put in the QR when the PC has several interfaces (an IP literal, not a hostname); `--yes` approves the first phone without asking.
3. The terminal prints an `openrelay://pair?v=1&h=…` URI and a QR. In the app, tap **Emparejar con PC**, then paste the link or scan the QR (**Escanear QR**) and tap **Emparejar**.
4. The phone shows **Esperando aprobación en la PC…**. The PC prints the device fingerprint (`Dispositivo: …`) and asks `¿Aprobar este dispositivo? [y/N]`; answer `y`.
5. The app switches to **Emparejado con** the host address. Start capture: the app shows **Enviando a** the host and the PC's `openrelay status` reports one session with `pipewire: {"ok": true}`.
6. On Linux, choose **`openrelay-mic`** as the microphone in your conferencing or recording app. `openrelay devices` lists the paired identity and its capabilities.

The pairing window closes on rejection, expiry, Ctrl-C, EOF or a QR failure, so a later `openrelay pair` is never blocked by a stale window. After approval it stays open for a few seconds so the phone can receive its client certificate. See [docs/security/pairing.md](docs/security/pairing.md) for what the QR does and does not trust.

The app and the CLI prompts are in Spanish.

## Build from source

### Linux

```sh
# Ubuntu 24.04 / Debian 12
sudo apt install build-essential pkg-config clang libclang-dev cmake git \
  libpipewire-0.3-dev libspa-0.2-dev libpulse-dev libopus-dev
# Fedora
sudo dnf install gcc clang clang-devel pkgconf cmake git \
  pipewire-devel pulseaudio-libs-devel opus-devel
# Arch
sudo pacman -S base-devel clang cmake git pipewire libpulse opus

# Rust via rustup; the repo pins 1.80.1 in rust-toolchain.toml
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

git clone https://github.com/MrFaviod/OpenRelay && cd OpenRelay
cargo build --release --locked -p openrelay-cli
install -Dm0755 target/release/openrelay ~.local/bin/openrelay
```

Run the tests:

```sh
cargo test --locked --workspace
```

`bash scripts/package.sh --check` builds the tarball in a temporary directory and checks permissions, contents and reproducibility. It installs nothing.

### Android from source

You need JDK 17, the Android SDK (platform 35, build-tools 35.0.0), **NDK 27.2.12479018** and `rustup` with the Android targets.

```sh
rustup target add --toolchain 1.80.1 aarch64-linux-android x86_64-linux-android
echo "sdk.dir=$ANDROID_HOME" > android/local.properties

# 1. Rust JNI bridge -> android/rust/target/jniLibs/<abi>/libopenrelay_bridge.so
NDK=$ANDROID_HOME/ndk/27.2.12479018 bash android/rust/build.sh

# 2. The APK
cd android &&./gradlew assembleDebug
# -> android/app/build/outputs/apk/debug/app-debug.apk
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

The NDK toolchain path in `build.sh` assumes a `linux-x86_64` host.

## Uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | OPENRELAY_UNINSTALL=1 sh
```

Or by hand:

```sh
systemctl --user disable --now openrelay.service
rm -f ~.local/bin/openrelay ~.config/systemd/user/openrelay.service
systemctl --user daemon-reload
```

## Troubleshooting

**`error while loading shared libraries: libpipewire-0.3.so.0`**
Install the runtime libraries listed under [Requirements](#requirements). If your PipeWire is older than 0.3.65 (Ubuntu 22.04, for example), the prebuilt binary will not work. Upgrade the distro or build from source against a newer PipeWire.

**`version 'GLIBC_2.34' not found`**
Your distro is older than the release baseline. Build from source.

**`openrelay status` exits with 1**
The daemon isn't running, or it is using a different socket. Start it with `openrelay daemon`, or check `systemctl --user status openrelay.service`. The default socket is `$XDG_RUNTIME_DIR/openrelay/control.sock`. If `XDG_RUNTIME_DIR` is unset, pass `--socket PATH`.

**`openrelay: command not found` right after installing**
`~.local/bin` is not on your `PATH`. Add `export PATH="$HOME.local/bin:$PATH"` to your shell profile.

**`busy`: pairing window already open**
Another `openrelay pair` still owns the single enrollment window (the daemon allows one at a time). Wait for it to expire, or finish it on the other terminal. Ctrl-C, EOF on stdin and a `n` answer now close the window immediately; a window whose CLI was killed outright is reaped by the daemon as soon as it notices the process is gone. If none of that applies, the port is held by another process: `ss -lunp | grep 47001`.

**Phone stuck on "Esperando aprobación en la PC…"**
The phone reached the PC but the PC side never saw the enrollment. Check, in order: the host firewall allows UDP 47000-47001 from the phone's LAN; both devices really are on the same subnet (guest Wi-Fi and VLAN isolation break this); the QR advertised the right address — on a PC with several interfaces pass `--advertise 192.168.1.20` (an IP literal; hostnames are rejected). `openrelay status` should show `"pair": {"open": true, "pending": 0...}` while you wait; a `pending` of 1 means the PC already has it and is asking for approval.

**`openrelay-mic` does not appear as a microphone**
The source is created by the daemon inside PipeWire; it needs a running PipeWire session **and a session manager (WirePlumber) to link it to applications**. Check `pactl info | head -3` and `wpctl status`. Then look at `openrelay status`: `"pipewire": {"ok": false, "error":...}` names the failure, and the node cannot exist until the session is healthy.

**`invalid: advertise debe ser una IP`**
`--advertise` takes an IP address, not a DNS name. Use the PC's LAN IPv4, for example `--advertise 192.168.1.20`.
