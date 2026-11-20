#!/usr/bin/env bash
# Reproducible end-to-end test for OpenRelay on a real Linux desktop stack.
#
#   scripts/e2e/run.sh            # build the image, run the test, keep artifacts
#   KEEP_IMAGE=1 scripts/e2e/run.sh
#
# What it proves: a synthetic phone enrolls with the pairing URI, streams a
# 440 Hz tone over the real QUIC session, and a normal PipeWire consumer
# (`pw-record`, plus the PulseAudio API through pipewire-pulse) receives that
# tone from `openrelay-mic`. It also proves `openrelay devices remove` revokes
# the identity.
#
# Requirements on the host: docker with --network host support and network
# access for the first build (Ubuntu packages + rustup).
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
IMAGE=${IMAGE:-openrelay-e2e:24.04}
TARGET_VOLUME=${TARGET_VOLUME:-openrelay-e2e-target}
ARTIFACTS=${ARTIFACTS:-$HERE/artifacts}

mkdir -p "$ARTIFACTS"

echo "==> building $IMAGE"
docker build -t "$IMAGE" -f "$HERE/Dockerfile" "$REPO"

echo "==> running the end-to-end test (artifacts in $ARTIFACTS)"
set +e
docker run --rm --network host --user 0 \
    -v "$REPO:/src:ro" \
    -v "$TARGET_VOLUME:/build" \
    -v "$ARTIFACTS:/out" \
    -e OUT=/out \
    -e BUILD_DIR=/build \
    "$IMAGE" /src/scripts/e2e/container.sh 2>&1 | tee "$ARTIFACTS/console.log"
status=${PIPESTATUS[0]}
set -e

if [ "${KEEP_IMAGE:-0}" != 1 ]; then
    echo "==> image kept as $IMAGE (KEEP_IMAGE is informational only)"
fi

if [ "$status" = 0 ]; then
    echo "==> E2E PASS"
else
    echo "==> E2E FAIL (exit $status); logs in $ARTIFACTS" >&2
fi
exit "$status"
