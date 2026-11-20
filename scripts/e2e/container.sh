#!/usr/bin/env bash
# In-container half of the OpenRelay desktop-Linux end-to-end test.
#
# Runs inside an ubuntu:24.04 image with a real PipeWire + WirePlumber +
# pipewire-pulse session, installs the daemon the way a user does, pairs a
# synthetic phone, records from `openrelay-mic` and checks the recording
# carries the 440 Hz tone. Finally it removes the device and proves the
# removed identity can no longer open a session.
#
# Every check prints `PASS:` or `FAIL:`; the script exits non-zero if any
# check failed.
set -uo pipefail

OUT=${OUT:-/out}
BUILD_DIR=${BUILD_DIR:-/build}

# `docker run` starts as root. PipeWire and WirePlumber are exercised as the
# ordinary desktop user, and the build/artifact volumes have to belong to it.
if [ "$(id -u)" = 0 ]; then
    mkdir -p "$OUT" "$BUILD_DIR"
    chown -R relay:relay "$OUT" "$BUILD_DIR" 2>/dev/null || true
    # `su -` y no `su`: un login resetea HOME a /home/relay. Sin eso el shell
    # hereda HOME=/root y los XDG dirs quedan fuera del alcance de relay.
    exec su - relay -s /bin/bash -c "OUT='$OUT' BUILD_DIR='$BUILD_DIR' /src/scripts/e2e/container.sh"
fi

mkdir -p "$OUT"

export XDG_RUNTIME_DIR="$HOME/runtime"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME"
chmod 700 "$XDG_RUNTIME_DIR"
export PATH="$HOME/.local/bin:$PATH"

FAILED=0
pids=()
pass() { printf 'PASS: %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*"; FAILED=1; }
step() { printf '\n===== %s =====\n' "$*"; }
cleanup() {
    for pid in "${pids[@]:-}"; do
        [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null
    done
    wait 2>/dev/null
}
trap cleanup EXIT

step "1. PipeWire desktop session"
DBUS_INFO=$(dbus-daemon --session --fork --print-address=1 --print-pid=1)
DBUS_ADDRESS=$(printf '%s\n' "$DBUS_INFO" | sed -n 1p)
DBUS_PID=$(printf '%s\n' "$DBUS_INFO" | sed -n 2p)
pids+=("$DBUS_PID")
export DBUS_SESSION_BUS_ADDRESS="$DBUS_ADDRESS"
pass "D-Bus session bus on $DBUS_ADDRESS"

LAST_BG=
start_bg() { # log command...
    local log=$1
    shift
    "$@" >"$OUT/$log" 2>&1 &
    LAST_BG=$!
    pids+=("$LAST_BG")
}

# Never capture a PID with `$(...)`: the background child dies when the
# command-substitution subshell exits.
start_bg pipewire.log pipewire
PW_PID=$LAST_BG
for _ in $(seq 1 100); do pw-cli info 0 >/dev/null 2>&1 && break; sleep 0.1; done
if pw-cli info 0 >/dev/null 2>&1; then
    pass "PipeWire daemon is up (pid $PW_PID)"
else
    fail "PipeWire did not start"
    tail -20 "$OUT/pipewire.log"
    exit 1
fi

# WirePlumber is the session manager a normal desktop runs. It may complain
# about missing udev/seat in a container, but it still owns the default nodes.
start_bg wireplumber.log wireplumber
WP_PID=$LAST_BG
sleep 1
if kill -0 "$WP_PID" 2>/dev/null; then
    pass "WirePlumber session manager is running (pid $WP_PID)"
else
    fail "WirePlumber exited; see wireplumber.log"
    tail -20 "$OUT/wireplumber.log"
fi

start_bg pipewire-pulse.log pipewire-pulse
PULSE_PID=$LAST_BG
for _ in $(seq 1 50); do pactl info >/dev/null 2>&1 && break; sleep 0.1; done
if pactl info >/dev/null 2>&1; then
    pass "pipewire-pulse answers pactl (PulseAudio API)"
else
    fail "pipewire-pulse did not answer pactl"
fi

step "2. Build and install from source"
cd /src || exit 1
export CARGO_TARGET_DIR="$BUILD_DIR"
if cargo build --locked --release -p openrelay-cli >"$OUT/build-cli.log" 2>&1; then
    pass "cargo build --locked --release -p openrelay-cli"
else
    fail "release build failed"
    tail -20 "$OUT/build-cli.log"
    exit 1
fi
if cargo build --locked --release -p openrelay-transport --example fake_phone \
    >"$OUT/build-phone.log" 2>&1; then
    pass "cargo build --locked --release -p openrelay-transport --example fake_phone"
else
    fail "fake_phone build failed"
    tail -20 "$OUT/build-phone.log"
    exit 1
fi

# install.sh downloads a release tarball and verifies SHA256SUMS. The harness
# builds that same tarball locally and points install.sh at it with
# OPENRELAY_BASE_URL, so the install path under test is the real one.
VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
case "$(uname -m)" in
    aarch64 | arm64) TARGET=aarch64-unknown-linux-gnu ;;
    x86_64 | amd64) TARGET=x86_64-unknown-linux-gnu ;;
    *) fail "unsupported architecture $(uname -m)"; exit 1 ;;
esac
ASSET="openrelay-$VERSION-$TARGET.tar.gz"
DIST="$OUT/dist"
rm -rf "$DIST"
mkdir -p "$DIST/openrelay-$VERSION"
install -m 755 "$BUILD_DIR/release/openrelay" "$DIST/openrelay-$VERSION/openrelay"
install -m 644 /src/packaging/systemd/openrelay.service "$DIST/openrelay-$VERSION/openrelay.service"
tar -czf "$DIST/$ASSET" -C "$DIST" "openrelay-$VERSION"
(cd "$DIST" && sha256sum "$ASSET" >SHA256SUMS)
if OPENRELAY_VERSION="v$VERSION" OPENRELAY_BASE_URL="file://$DIST" \
    OPENRELAY_NO_UNIT=1 sh /src/install.sh >"$OUT/install.log" 2>&1; then
    pass "install.sh installed $(command -v openrelay) from local tarball"
else
    fail "install.sh failed"
    tail -20 "$OUT/install.log"
    exit 1
fi
if [ -x "$HOME/.local/bin/openrelay" ] && "$HOME/.local/bin/openrelay" --help >/dev/null; then
    pass "installed ~/.local/bin/openrelay from $ASSET"
else
    fail "installed binary does not run"
    exit 1
fi

step "3. Daemon, pairing and synthetic phone"
openrelay daemon >"$OUT/daemon.log" 2>&1 &
pids+=($!)
DAEMON_PID=$!
for _ in $(seq 1 100); do openrelay status >/dev/null 2>&1 && break; sleep 0.1; done
if openrelay status >"$OUT/status-initial.json" 2>&1; then
    pass "daemon answers status: $(tr -d '\n' <"$OUT/status-initial.json" | cut -c1-120)"
else
    fail "daemon did not answer status"
    tail -20 "$OUT/daemon.log"
    exit 1
fi

openrelay pair --ttl 90 --advertise 127.0.0.1 --yes >"$OUT/pair.log" 2>&1 &
pids+=($!)
URI=""
for _ in $(seq 1 100); do
    URI=$(sed -n '1p' "$OUT/pair.log")
    case "$URI" in openrelay://pair*) break ;; esac
    sleep 0.1
done
case "$URI" in
    openrelay://pair*) pass "pair window printed $URI" ;;
    *)
        fail "pair did not print a URI"
        cat "$OUT/pair.log"
        exit 1
        ;;
esac

PHONE="$BUILD_DIR/release/examples/fake_phone"
"$PHONE" --uri "$URI" --seconds 15 --save-identity "$OUT/fake-phone.json" \
    >"$OUT/fake-phone.log" 2>&1 &
pids+=($!)
PHONE_PID=$!

AUTHORIZED=0
for _ in $(seq 1 150); do
    if /usr/bin/grep -q "autorizada" "$OUT/fake-phone.log" 2>/dev/null; then
        AUTHORIZED=1
        break
    fi
    sleep 0.1
done
if [ "$AUTHORIZED" = 1 ]; then
    pass "fake phone enrolled and session authorized"
else
    fail "fake phone never got an authorized session"
    cat "$OUT/fake-phone.log"
fi

# Wait for the PipeWire node to be published.
NODE_SEEN=0
for _ in $(seq 1 100); do
    if pw-cli ls Node 2>/dev/null | /usr/bin/grep -q 'node.name = "openrelay-mic"'; then
        NODE_SEEN=1
        break
    fi
    sleep 0.1
done
pw-cli ls Node >"$OUT/nodes.txt" 2>&1
if [ "$NODE_SEEN" = 1 ]; then
    pass "pw-cli ls Node publishes openrelay-mic"
else
    fail "openrelay-mic is not in the PipeWire graph"
fi

if /usr/bin/grep -q 'media.class = "Audio/Source"' "$OUT/nodes.txt"; then
    pass "openrelay-mic media.class is Audio/Source"
else
    fail "openrelay-mic is not an Audio/Source"
fi

SESSIONS=0
for _ in $(seq 1 30); do
    if openrelay status 2>/dev/null | /usr/bin/grep -Eq '"sessions":[[:space:]]*1'; then
        SESSIONS=1
        break
    fi
    sleep 0.1
done
openrelay status >"$OUT/status-streaming.json" 2>&1
if [ "$SESSIONS" = 1 ]; then
    pass "daemon status reports 1 authorized session"
else
    fail "daemon status does not report one session"
    cat "$OUT/status-streaming.json"
fi

step "4. Record from openrelay-mic with PipeWire and with PulseAudio"
timeout -s INT 5 pw-record --target openrelay-mic --rate 48000 --channels 1 \
    --format s16 "$OUT/out.wav" >"$OUT/pw-record.log" 2>&1 &
REC_PID=$!
sleep 1
pactl list short sources >"$OUT/pactl-sources.txt" 2>&1
wpctl status >"$OUT/wpctl.txt" 2>&1
pw-cli ls Node >"$OUT/nodes-streaming.txt" 2>&1
wait "$REC_PID"
REC_EXIT=$?
if [ "$REC_EXIT" = 0 ] || [ "$REC_EXIT" = 124 ] || [ "$REC_EXIT" = 130 ]; then
    pass "pw-record exited $REC_EXIT after the 5 s window"
else
    fail "pw-record exited $REC_EXIT"
    tail -20 "$OUT/pw-record.log"
fi

if /usr/bin/grep -q openrelay-mic "$OUT/pactl-sources.txt"; then
    pass "pactl lists openrelay-mic: $(/usr/bin/grep openrelay-mic "$OUT/pactl-sources.txt")"
else
    fail "pactl does not list openrelay-mic"
fi

if /usr/bin/grep -q openrelay-mic "$OUT/wpctl.txt"; then
    pass "wpctl status shows openrelay-mic"
fi

python3 /src/scripts/e2e/analyze.py "$OUT/out.wav" | tee "$OUT/analysis.txt"
if /usr/bin/grep -q '^PASS:' "$OUT/analysis.txt"; then
    pass "pw-record audio carries the 440 Hz tone"
else
    fail "pw-record audio did not carry the tone"
fi

# Same graph, but through the PulseAudio compatibility server: `parec` must
# see the Source too and get the same tone.
timeout -s INT 4 parec -d openrelay-mic --rate=48000 --channels=1 \
    --format=s16le --file-format=wav "$OUT/pulse.wav" >"$OUT/parec.log" 2>&1
PAREC_EXIT=$?
if [ "$PAREC_EXIT" = 0 ] || [ "$PAREC_EXIT" = 124 ] || [ "$PAREC_EXIT" = 130 ]; then
    pass "parec exited $PAREC_EXIT after the 4 s window"
else
    fail "parec exited $PAREC_EXIT"
    tail -20 "$OUT/parec.log"
fi
python3 /src/scripts/e2e/analyze.py "$OUT/pulse.wav" | tee "$OUT/pulse-analysis.txt"
if /usr/bin/grep -q '^PASS:' "$OUT/pulse-analysis.txt"; then
    pass "parec audio carries the 440 Hz tone"
else
    fail "parec audio did not carry the tone"
fi

# The phone keeps streaming for 12 s; give the daemon time to log counters.
sleep 1
python3 - "$XDG_RUNTIME_DIR/openrelay/control.sock" >"$OUT/audio-stats.json" <<'PY'
import json, socket, sys
sock = socket.socket(socket.AF_UNIX)
sock.connect(sys.argv[1])
sock.sendall((json.dumps({"request_id": "e2e", "op": "audio.stats"}) + "\n").encode())
data = b""
while not data.endswith(b"\n"):
    chunk = sock.recv(65536)
    if not chunk:
        break
    data += chunk
print(data.decode().strip())
PY
cat "$OUT/audio-stats.json"
if /usr/bin/grep -q '"pcm16_frames":' "$OUT/audio-stats.json" && ! /usr/bin/grep -Eq '"sessions":[[:space:]]*\[\]' "$OUT/audio-stats.json"; then
    pass "audio.stats reports a live session with frame counters"
else
    fail "audio.stats has no session"
fi

wait "$PHONE_PID" 2>/dev/null
step "5. devices.remove"
openrelay devices >"$OUT/devices-before.txt" 2>"$OUT/devices-before.err"
cat "$OUT/devices-before.txt"
FINGERPRINT=$(sed -n '1p' "$OUT/devices-before.txt" | awk '{print $1}')
if [ -n "$FINGERPRINT" ] && [ ${#FINGERPRINT} -eq 64 ]; then
    pass "devices lists the approved fingerprint"
else
    fail "devices did not list the approved fingerprint"
fi

if openrelay devices remove "$FINGERPRINT" >"$OUT/devices-remove.txt" 2>&1; then
    cat "$OUT/devices-remove.txt"
    pass "devices remove <fingerprint> succeeded"
else
    cat "$OUT/devices-remove.txt"
    fail "devices remove failed"
fi

openrelay devices >"$OUT/devices-after.txt" 2>"$OUT/devices-after.err"
if [ -s "$OUT/devices-after.txt" ]; then
    fail "device still listed after removal"
    cat "$OUT/devices-after.txt"
else
    pass "device list is empty after removal ($(cat "$OUT/devices-after.err"))"
fi

if /usr/bin/grep -q "$FINGERPRINT" "$XDG_CONFIG_HOME/openrelay/identities.json" 2>/dev/null; then
    fail "identities.json still contains the removed pin"
else
    pass "identities.json no longer contains the removed pin"
fi

LIVE=1
for _ in $(seq 1 30); do
    if openrelay status 2>/dev/null | /usr/bin/grep -Eq '"sessions":[[:space:]]*0'; then
        LIVE=0
        break
    fi
    sleep 0.1
done
if [ "$LIVE" = 0 ]; then
    pass "removing the device closed its live session (status sessions 0)"
else
    fail "the removed device still has a live session"
    openrelay status >"$OUT/status-after-remove.json" 2>&1
    cat "$OUT/status-after-remove.json"
fi

if "$PHONE" --identity "$OUT/fake-phone.json" --expect-refused >"$OUT/fake-phone-refused.log" 2>&1 \
    && /usr/bin/grep -q 'REFUSED: .*unauthorized' "$OUT/fake-phone-refused.log" \
    && ! /usr/bin/grep -q 'sesión autorizada' "$OUT/fake-phone-refused.log"; then
    cat "$OUT/fake-phone-refused.log"
    pass "removed identity is rejected at session authorization (connect refused/unauthorized)"
else
    cat "$OUT/fake-phone-refused.log"
    fail "removed identity could still connect or was not refused at authorization"
fi

python3 - "$XDG_RUNTIME_DIR/openrelay/control.sock" >"$OUT/status-final.json" <<'PY'
import json, socket, sys
sock = socket.socket(socket.AF_UNIX)
sock.connect(sys.argv[1])
sock.sendall((json.dumps({"request_id": "e2e", "op": "status"}) + "\n").encode())
data = b""
while not data.endswith(b"\n"):
    chunk = sock.recv(65536)
    if not chunk:
        break
    data += chunk
print(data.decode().strip())
PY
cat "$OUT/status-final.json"

step "Result"
if [ "$FAILED" = 0 ]; then
    echo "E2E: PASS"
    exit 0
fi
echo "E2E: FAIL"
exit 1
