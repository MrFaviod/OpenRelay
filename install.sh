#!/bin/sh
# OpenRelay installer for Linux (x86_64 / aarch64).
#
#   curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | sh
#
# Installs the `openrelay` binary into ~/.local/bin (no root) and the optional
# systemd --user unit into ~/.config/systemd/user. Every download is checked
# against the release SHA256SUMS before anything is installed.
#
# Environment:
#   OPENRELAY_VERSION   release tag to install (default: latest), e.g. v0.2.2
#   OPENRELAY_BASE_URL  download base (default: the GitHub release URL); a
#                       file:// directory with the same asset layout also works
#   OPENRELAY_BIN_DIR   install directory (default: ~/.local/bin)
#   OPENRELAY_NO_UNIT=1 skip the systemd --user unit
#   OPENRELAY_NO_SERVICE=1
#                       install the unit but do not enable or start it
#   OPENRELAY_FORCE=1   install even if required shared libraries are missing
#   OPENRELAY_UNINSTALL=1 remove a previous install and exit
#
# Update an existing install to the latest release:
#
#   curl -fsSL https://raw.githubusercontent.com/MrFaviod/OpenRelay/main/install.sh | sh -s -- --update
set -eu

REPO="MrFaviod/OpenRelay"
BIN_DIR="${OPENRELAY_BIN_DIR:-$HOME/.local/bin}"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"; }

update_only=0
for arg in "$@"; do
  case "$arg" in
    --update) update_only=1 ;;
    --uninstall) OPENRELAY_UNINSTALL=1 ;;
    *) die "unknown option: $arg (use --update or --uninstall)" ;;
  esac
done
if [ "$update_only" = 1 ] && [ ! -x "$BIN_DIR/openrelay" ]; then
  die "OpenRelay is not installed in $BIN_DIR; run the installer without --update"
fi

if [ "${OPENRELAY_UNINSTALL:-}" = 1 ]; then
  if command -v systemctl >/dev/null 2>&1 && [ -f "$UNIT_DIR/openrelay.service" ]; then
    systemctl --user disable --now openrelay.service 2>/dev/null || true
  fi
  rm -f "$BIN_DIR/openrelay" "$UNIT_DIR/openrelay.service"
  command -v systemctl >/dev/null 2>&1 && systemctl --user daemon-reload 2>/dev/null || true
  say "OpenRelay removed."
  exit 0
fi

[ "$(uname -s)" = Linux ] || die "OpenRelay's daemon runs on Linux only."
case "$(uname -m)" in
  x86_64 | amd64) target=x86_64-unknown-linux-gnu ;;
  aarch64 | arm64) target=aarch64-unknown-linux-gnu ;;
  *) die "unsupported architecture: $(uname -m) (x86_64 and aarch64 only)" ;;
esac

need curl
need tar
need sha256sum

tag="${OPENRELAY_VERSION:-}"
if [ -z "$tag" ]; then
  tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$tag" ] || die "could not resolve the latest release; set OPENRELAY_VERSION=vX.Y.Z"
fi
version="${tag#v}"
asset="openrelay-$version-$target.tar.gz"
base="${OPENRELAY_BASE_URL:-https://github.com/$REPO/releases/download/$tag}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading OpenRelay $tag for $target"
curl -fsSL -o "$tmp/$asset" "$base/$asset" || die "download failed: $base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

say "Verifying checksum"
sums=$(grep " $asset\$" "$tmp/SHA256SUMS" || true)
[ -n "$sums" ] || die "SHA256SUMS has no entry for $asset; nothing was installed"
(cd "$tmp" && printf '%s\n' "$sums" | sha256sum -c - >/dev/null) ||
  die "checksum mismatch for $asset; nothing was installed"

tar -xzf "$tmp/$asset" -C "$tmp"
src="$tmp/openrelay-$version"
[ -d "$src" ] || die "unexpected tarball layout: no openrelay-$version/ directory"

# Required runtime libraries: PipeWire >= 0.3.65 and libopus. Check the
# downloaded binary before installing anything; abort unless FORCE=1.
if command -v ldd >/dev/null 2>&1; then
  missing=$(ldd "$src/openrelay" 2>/dev/null | sed -n 's/^[[:space:]]*\([^ ]*\) => not found/\1/p')
  if [ -n "$missing" ]; then
    warn "the binary needs shared libraries that are not installed:"
    printf '         %s\n' $missing >&2
    warn "Debian/Ubuntu: sudo apt install libpipewire-0.3-0t64 libopus0   (older: libpipewire-0.3-0)"
    warn "Fedora:        sudo dnf install pipewire-libs opus"
    warn "Arch:          sudo pacman -S libpipewire opus"
    if [ "${OPENRELAY_FORCE:-}" != 1 ]; then
      die "nothing was installed; install the libraries or re-run with OPENRELAY_FORCE=1"
    fi
    warn "OPENRELAY_FORCE=1: installing anyway"
  fi
fi

updating=0
[ -x "$BIN_DIR/openrelay" ] && updating=1
mkdir -p "$BIN_DIR"
install -m 0755 "$src/openrelay" "$BIN_DIR/openrelay"
if [ "$updating" = 1 ]; then
  say "Updated $BIN_DIR/openrelay to $tag"
else
  say "Installed $BIN_DIR/openrelay"
fi

if [ "${OPENRELAY_NO_UNIT:-}" != 1 ]; then
  mkdir -p "$UNIT_DIR"
  install -m 0644 "$src/openrelay.service" "$UNIT_DIR/openrelay.service"
  if [ "$BIN_DIR" != "$HOME/.local/bin" ]; then
    # The packaged unit points at %h/.local/bin; keep it on the chosen path.
    escaped=$(printf '%s' "$BIN_DIR/openrelay" | sed 's/[\\&|]/\\&/g')
    sed -i "s|^ExecStart=.*|ExecStart=\"$escaped\" daemon|" "$UNIT_DIR/openrelay.service"
  fi
  say "Installed $UNIT_DIR/openrelay.service"
fi

started=0
if [ "${OPENRELAY_NO_UNIT:-}" = 1 ]; then
  say "No service installed; start the daemon with: openrelay daemon"
elif [ "${OPENRELAY_NO_SERVICE:-}" = 1 ]; then
  say "Service not started (OPENRELAY_NO_SERVICE=1); start it with: systemctl --user enable --now openrelay.service"
elif command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  systemctl --user daemon-reload || true
  # enable --now does not reload a running daemon, so restart it to pick up
  # the new binary when this is an update.
  if systemctl --user enable openrelay.service &&
    systemctl --user restart openrelay.service; then
    i=0
    while [ "$i" -lt 10 ]; do
      if "$BIN_DIR/openrelay" status >/dev/null 2>&1; then
        started=1
        say "OpenRelay daemon is running."
        break
      fi
      i=$((i + 1))
      sleep 0.5
    done
  fi
  if [ "$started" != 1 ]; then
    warn "the openrelay service did not become ready; the install is complete"
    systemctl --user status openrelay.service --no-pager 2>&1 | tail -n 20 >&2 || true
    warn "logs: journalctl --user -u openrelay.service -e"
  fi
else
  say "No systemd user session. Start it with: openrelay daemon"
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not in your PATH. Add it: export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if [ "$updating" = 1 ]; then
  cat <<EOF

  OpenRelay is updated to $tag. Paired phones and settings are kept.

  Status:     openrelay status
  If you run the daemon by hand (openrelay daemon), restart it now.
EOF
  exit 0
fi

cat <<EOF

  OpenRelay $tag is installed.

  Next step:  openrelay pair
  Status:     openrelay status
  Service:    systemctl --user status openrelay.service   (or: openrelay daemon)

  Quick guide: https://github.com/$REPO/blob/main/docs/cli-quickstart.md
EOF
