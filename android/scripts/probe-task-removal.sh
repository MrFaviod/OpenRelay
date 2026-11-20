#!/usr/bin/env bash
# Retirada de tarea REAL: swipe de la tarjeta en Recents con el FGS vivo.
# La instrumentación de espera muere al quitar la tarea; el veredicto sale de dumpsys externo.
# Uso: probe-task-removal.sh <serial> <out-dir>
set -uo pipefail
PKG=dev.openrelay.app; TEST_PKG=dev.openrelay.app.test; RUNNER=androidx.test.runner.AndroidJUnitRunner
S="${1:?serial}"; OUT="${2:?out}"; mkdir -p "$OUT"
ADB="${ADB:-${ANDROID_HOME:-$HOME/Android/Sdk}/platform-tools/adb}"
LOCK="$HOME/.cache/openrelay-emu/adb.lock"
adb_s(){ "$ADB" -s "$S" "$@"; }
exec 9>"$LOCK"; flock 9
log(){ echo "$*" | tee -a "$OUT/probe.log"; }
adb_s shell am force-stop "$PKG"; adb_s shell am force-stop "$TEST_PKG"
adb_s shell run-as "$PKG" rm -f files/holdFgsForExternalTaskRemoval.xml >/dev/null 2>&1
adb_s shell am instrument -w -r -e class "dev.openrelay.app.LifecycleActionsTest#holdFgsForExternalTaskRemoval" \
  -e listener dev.openrelay.app.FgsXmlListener -e fgsXmlName holdFgsForExternalTaskRemoval.xml \
  "$TEST_PKG/$RUNNER" > "$OUT/hold.instrument" 2>&1 &
INSTR_PID=$!
ok=0
for i in $(seq 1 40); do
  if adb_s shell dumpsys activity services "$PKG" | grep -q "isForeground=true"; then ok=1; break; fi
  sleep 0.5
done
log "fgs_foreground_before=$ok"
adb_s shell dumpsys activity services "$PKG" > "$OUT/services-before.txt"
adb_s shell dumpsys activity recents > "$OUT/recents-before.txt"
# Recents: abrir y volcar; la tarjeta de la app está en [467,207][1080,2006].
adb_s shell input keyevent KEYCODE_APP_SWITCH; sleep 2
adb_s shell uiautomator dump /sdcard/rt.xml >/dev/null
adb_s shell cat /sdcard/rt.xml > "$OUT/recents-ui.xml"
# Swipe de la tarjeta hacia arriba (descarte real de la tarea).
adb_s shell input swipe 700 900 700 100 250
sleep 3
adb_s shell dumpsys activity services "$PKG" > "$OUT/services-after.txt"
adb_s shell dumpsys activity recents > "$OUT/recents-after.txt"
adb_s shell dumpsys notification --noredact > "$OUT/notif-after.txt"
log "fgs_foreground_after=$(grep -c 'isForeground=true' "$OUT/services-after.txt")"
log "app_task_after=$(grep -c 'dev.openrelay.app' "$OUT/recents-after.txt")"
wait "$INSTR_PID"; log "hold_instrument_exit=$?"
