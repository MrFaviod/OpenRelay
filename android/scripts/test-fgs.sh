#!/usr/bin/env bash
# FgsLifecycleTest, un proceso de instrumentación por método.
#
# Revocar RECORD_AUDIO o POST_NOTIFICATIONS desde el proceso del test mata
# la instrumentación en API 34. Los permisos se preparan aquí, con la app
# detenida, y cada método corre solo. No se desinstala entre métodos.
#
# Uso: android/scripts/test-fgs.sh -s <serial> [--out DIR]
# El serial es obligatorio: nunca se opera el adb por defecto.
set -euo pipefail

PKG=dev.openrelay.app
TEST_PKG=dev.openrelay.app.test
CLASS=dev.openrelay.app.FgsLifecycleTest
RUNNER=androidx.test.runner.AndroidJUnitRunner
RECORD_AUDIO=android.permission.RECORD_AUDIO
POST_NOTIFICATIONS=android.permission.POST_NOTIFICATIONS
LOCK="${OPENRELAY_ADB_LOCK:-$HOME/.cache/openrelay-emu/adb.lock}"

SERIAL=""
OUT=""
while [ $# -gt 0 ]; do
  case "$1" in
    -s) SERIAL="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    *) echo "uso: $0 -s <serial> [--out DIR]" >&2; exit 2 ;;
  esac
done
if [ -z "$SERIAL" ]; then
  echo "falta -s <serial>; no se usa el dispositivo por defecto" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
if [ -z "$OUT" ]; then
  OUT="$ROOT/t19/fgs-$(date +%Y%m%dT%H%M%S)"
fi
mkdir -p "$OUT" "$(dirname "$LOCK")"
LOG="$OUT/runner.log"

# Métodos en el orden de la clase. El de notificaciones no es runtime
# antes de API 33: el propio test lo marca assumption-not-applicable y
# este runner lo saca del conteo de ejecución.
METHODS=(
  manifestDeclaresMicForegroundServiceAndNoLocalNetwork
  visibleActivityStartsForegroundMicWithStopAndMute
  withoutRecordAudioItDoesNotRunAndSaysWhy
  deniedNotificationsDoNotBlockTheForegroundService
  hiddenActivityDoesNotStartTheMicrophone
)
DENY_RECORD=withoutRecordAudioItDoesNotRunAndSaysWhy
DENY_NOTIF=deniedNotificationsDoNotBlockTheForegroundService

ADB="${ANDROID_HOME:-$HOME/Android/Sdk}/platform-tools/adb"
if [ ! -x "$ADB" ]; then
  echo "adb no ejecutable: $ADB" >&2
  exit 2
fi

exec 9>"$LOCK"
if ! flock -n 9; then
  echo "adb.lock ocupado: $LOCK" >&2
  exit 2
fi

fail() {
  echo "FALLO: $*" | tee -a "$LOG" >&2
  exit 1
}

adb_s() {
  "$ADB" -s "$SERIAL" "$@"
}

# Estado de un permiso runtime: granted | denied | absent.
# absent = el paquete no lo tiene como runtime (API < 33 para notificaciones).
perm_state() {
  local perm="$1" line
  line="$(adb_s shell dumpsys package "$PKG" | tr -d '\r' \
    | grep -F "$perm: granted=" | head -1 || true)"
  case "$line" in
    *"granted=true"*) echo granted ;;
    *"granted=false"*) echo denied ;;
    "") echo absent ;;
    *) echo "unknown:$line" ;;
  esac
}

save_initial_perms() {
  INITIAL_RECORD="$(perm_state "$RECORD_AUDIO")"
  INITIAL_NOTIF="$(perm_state "$POST_NOTIFICATIONS")"
  {
    echo "serial=$SERIAL"
    echo "initial_record_audio=$INITIAL_RECORD"
    echo "initial_post_notifications=$INITIAL_NOTIF"
  } | tee "$OUT/initial-perms.txt"
}

restore_initial_perms() {
  local rc=0
  adb_s shell am force-stop "$PKG" || rc=$?
  case "${INITIAL_RECORD:-}" in
    granted) adb_s shell pm grant "$PKG" "$RECORD_AUDIO" || rc=$? ;;
    denied) adb_s shell pm revoke "$PKG" "$RECORD_AUDIO" || rc=$? ;;
  esac
  case "${INITIAL_NOTIF:-}" in
    granted) adb_s shell pm grant "$PKG" "$POST_NOTIFICATIONS" || rc=$? ;;
    denied) adb_s shell pm revoke "$PKG" "$POST_NOTIFICATIONS" || rc=$? ;;
  esac
  adb_s shell am force-stop "$PKG" || rc=$?
  {
    echo "restored_record_audio=$(perm_state "$RECORD_AUDIO")"
    echo "restored_post_notifications=$(perm_state "$POST_NOTIFICATIONS")"
    echo "restore_rc=$rc"
  } | tee -a "$OUT/initial-perms.txt"
  return "$rc"
}

on_exit() {
  local status=$?
  if [ -n "${INITIAL_RECORD:-}" ]; then
    if ! restore_initial_perms; then
      echo "FALLO: no se restauraron los permisos iniciales" | tee -a "$LOG" >&2
      status=1
    fi
  fi
  echo "runner_exit=$status" | tee -a "$LOG"
  exit "$status"
}
trap on_exit EXIT

# Cuenta tests/failures/errors/skipped del XML. Vacío o sin <testsuite> es fallo.
# Imprime: tests failures errors skipped
xml_counts() {
  local xml="$1" method="$2"
  python3 - "$xml" "$method" <<'PY'
import sys
import xml.etree.ElementTree as ET
path = sys.argv[1]
try:
    root = ET.parse(path).getroot()
except ET.ParseError as exc:
    sys.stderr.write(f"xml ilegible {path}: {exc}\n")
    sys.exit(3)
suite = root if root.tag == "testsuite" else root.find("testsuite")
if suite is None:
    sys.stderr.write(f"sin testsuite en {path}\n")
    sys.exit(3)
def num(name):
    raw = suite.get(name)
    if raw is None or raw == "":
        sys.stderr.write(f"falta {name} en {path}\n")
        sys.exit(3)
    return int(raw)
tests, failures, errors, skipped = (num(n) for n in
    ("tests", "failures", "errors", "skipped"))
cases = [c for c in suite.iter("testcase")]
if len(cases) != tests:
    sys.stderr.write(f"{path}: tests={tests} pero hay {len(cases)} testcase\n")
    sys.exit(3)
want = sys.argv[2]
if len(cases) != 1:
    sys.stderr.write(f"{path}: se espera exactamente 1 testcase, hay {len(cases)}\n")
    sys.exit(3)
got = cases[0].get("name")
klass = cases[0].get("classname")
if got != want or klass != "dev.openrelay.app.FgsLifecycleTest":
    sys.stderr.write(
        f"{path}: testcase {klass}#{got}, se esperaba dev.openrelay.app.FgsLifecycleTest#{want}\n"
    )
    sys.exit(3)
print(f"{tests} {failures} {errors} {skipped}")
PY
}

echo "inicio serial=$SERIAL out=$OUT" | tee "$LOG"
boot="$(adb_s shell getprop sys.boot_completed | tr -d '\r')"
sdk="$(adb_s shell getprop ro.build.version.sdk | tr -d '\r')"
echo "boot_completed=$boot sdk=$sdk" | tee -a "$LOG"
[ "$boot" = "1" ] || fail "el emulador $SERIAL no terminó de arrancar"
case "$sdk" in
  ''|*[!0-9]*) fail "sdk ilegible: $sdk" ;;
esac

if ! adb_s shell pm path "$PKG" >/dev/null; then
  fail "falta $PKG; instalar una vez antes de este runner"
fi
if ! adb_s shell pm path "$TEST_PKG" >/dev/null; then
  fail "falta $TEST_PKG; instalar una vez antes de este runner"
fi

listed="$(adb_s shell pm list instrumentation | tr -d '\r' \
  | grep -F "instrumentation:$TEST_PKG/$RUNNER" || true)"
echo "instrumentation=$listed" | tee -a "$LOG"
[ -n "$listed" ] || fail "no está $TEST_PKG/$RUNNER; no se adivina otro runner"

save_initial_perms

ran=0
na=0
for method in "${METHODS[@]}"; do
  adb_s shell am force-stop "$PKG" || fail "force-stop antes de $method"
  adb_s shell am force-stop "$TEST_PKG" || true

  if [ "$method" = "$DENY_RECORD" ]; then
    adb_s shell pm revoke "$PKG" "$RECORD_AUDIO" || fail "revoke RECORD_AUDIO"
    adb_s shell pm grant "$PKG" "$POST_NOTIFICATIONS" || true
  elif [ "$method" = "$DENY_NOTIF" ]; then
    adb_s shell pm grant "$PKG" "$RECORD_AUDIO" || fail "grant RECORD_AUDIO"
    if [ "$sdk" -ge 33 ]; then
      adb_s shell pm revoke "$PKG" "$POST_NOTIFICATIONS" \
        || fail "revoke POST_NOTIFICATIONS"
    fi
  else
    adb_s shell pm grant "$PKG" "$RECORD_AUDIO" || fail "grant RECORD_AUDIO"
    if [ "$sdk" -ge 33 ]; then
      adb_s shell pm grant "$PKG" "$POST_NOTIFICATIONS" \
        || fail "grant POST_NOTIFICATIONS"
    fi
  fi
  echo "prep $method record=$(perm_state "$RECORD_AUDIO") notif=$(perm_state "$POST_NOTIFICATIONS")" \
    | tee -a "$LOG"

  xml="$OUT/${method}.xml"
  : >"$OUT/${method}.pull.err"
  # Un XML de una corrida anterior no es resultado de esta. Se borra en el
  # dispositivo y en local, y se comprueba que ya no está, antes de instrumentar.
  adb_s shell run-as "$PKG" rm -f "files/${method}.xml" \
    >/dev/null 2>>"$OUT/${method}.pull.err" || true
  if adb_s shell run-as "$PKG" cat "files/${method}.xml" \
      >"$OUT/${method}.stale" 2>/dev/null; then
    fail "$method: el XML del dispositivo sigue presente tras borrarlo"
  fi
  rm -f "$OUT/${method}.stale" "$xml"
  if [ -e "$xml" ]; then
    fail "$method: el XML local sigue presente tras borrarlo"
  fi

  set +e
  adb_s shell am instrument -w -r \
    -e class "$CLASS#$method" \
    -e listener dev.openrelay.app.FgsXmlListener \
    -e fgsXmlName "${method}.xml" \
    -e debug false \
    "$TEST_PKG/$RUNNER" >"$OUT/${method}.instrument" 2>&1
  inst_rc=$?
  set -e
  echo "instrument_exit $method=$inst_rc" | tee -a "$LOG"
  if [ "$inst_rc" -ne 0 ]; then
    fail "$method instrument exit=$inst_rc; ver ${method}.instrument"
  fi

  # El listener escribe en el filesDir de la app. Sin ese archivo no hay
  # resultado: el texto "OK" del runner no cuenta, ni un XML previo.
  adb_s shell run-as "$PKG" cat "files/${method}.xml" >"$xml" \
    2>>"$OUT/${method}.pull.err" || true
  if [ ! -s "$xml" ]; then
    fail "$method no dejó XML; ver ${method}.instrument"
  fi

  counts="$(xml_counts "$xml" "$method")" || fail "$method XML no acredita el testcase esperado"
  set -- $counts
  tests="$1" failures="$2" errors="$3" skipped="$4"
  echo "xml $method tests=$tests failures=$failures errors=$errors skipped=$skipped" \
    | tee -a "$LOG"

  if [ "$tests" -ne 1 ]; then
    fail "$method debía ejecutar exactamente 1 test, ejecutó $tests"
  fi
  if [ "$failures" -ne 0 ] || [ "$errors" -ne 0 ]; then
    fail "$method failures=$failures errors=$errors"
  fi

  if [ "$method" = "$DENY_NOTIF" ] && [ "$sdk" -lt 33 ]; then
    # No aplicable: tiene que figurar como skipped, nunca como PASS vacío.
    if [ "$skipped" -lt 1 ] || [ "$tests" -ne "$skipped" ]; then
      fail "$method en API $sdk debía ser no aplicable (skipped), no un PASS"
    fi
    na=$((na + 1))
  else
    if [ "$skipped" -ne 0 ]; then
      fail "$method no puede saltarse (skipped=$skipped)"
    fi
    ran=$((ran + 1))
  fi
done

expected=5
if [ "$sdk" -lt 33 ]; then
  expected=4
fi
if [ "$ran" -ne "$expected" ]; then
  fail "ejecutados=$ran, se esperaban $expected (no aplicables=$na)"
fi

{
  echo "sdk=$sdk"
  echo "serial=$SERIAL"
  echo "executed=$ran"
  echo "not_applicable=$na"
  echo "result=pass"
} | tee "$OUT/summary.txt"
echo "OK executed=$ran not_applicable=$na sdk=$sdk" | tee -a "$LOG"
