#!/usr/bin/env bash
# LifecycleActionsTest: botones de notificación por PendingIntent real y
# retirada de tarea real. Serial obligatorio. Todo adb bajo flock.
set -euo pipefail
PKG=dev.openrelay.app; TEST_PKG=dev.openrelay.app.test; RUNNER=androidx.test.runner.AndroidJUnitRunner
CLASS=dev.openrelay.app.LifecycleActionsTest
SERIAL="${1:?uso: $0 <serial> <out-dir> <method>}"; OUT="${2:?out}"; METHOD="${3:?method}"
ADB="${ADB:-${ANDROID_HOME:-$HOME/Android/Sdk}/platform-tools/adb}"
LOCK="$HOME/.cache/openrelay-emu/adb.lock"; mkdir -p "$OUT"
exec 9>"$LOCK"; flock 9
adb_s(){ "$ADB" -s "$SERIAL" "$@"; }
adb_s shell am force-stop "$PKG" || true
adb_s shell am force-stop "$TEST_PKG" || true
adb_s shell run-as "$PKG" rm -f "files/$METHOD.xml" >/dev/null 2>&1 || true
set +e
adb_s shell am instrument -w -r -e class "$CLASS#$METHOD" \
  -e listener dev.openrelay.app.FgsXmlListener -e fgsXmlName "$METHOD.xml" \
  -e debug false "$TEST_PKG/$RUNNER" > "$OUT/$METHOD.instrument" 2>&1
rc=$?; set -e
echo "instrument_exit=$rc"
adb_s shell run-as "$PKG" cat "files/$METHOD.xml" > "$OUT/$METHOD.xml" 2>"$OUT/$METHOD.pull.err" || true
echo "xml_bytes=$(stat -c %s "$OUT/$METHOD.xml" 2>/dev/null || echo 0)"
