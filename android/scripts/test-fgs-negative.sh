#!/usr/bin/env bash
# Prueba test-fgs.sh contra un adb falso. No habla con un emulador y no
# acredita la app: sólo que un XML viejo, una instrumentación cortada o un
# testcase que no es el método pedido no salen verdes.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RUNNER="$ROOT/android/scripts/test-fgs.sh"
BASE="$(mktemp -d)"
trap 'rm -rf "$BASE"' EXIT

write_fake() {
  local dev="$1" mode="$2"
  cat >"$BASE/fake-$mode" <<EOF
#!/usr/bin/env bash
set -euo pipefail
DEV="$dev"
MODE="$mode"
state() { sed -n "s/^\${1}=//p" "\$DEV/state"; }
shift; shift
cmd="\${1:-}"; shift || true
case "\$cmd" in
  shell)
    line="\$*"
    case "\$line" in
      "run-as dev.arl.audio rm -f "*)
        f="\${line##* }"
        if [ "\$MODE" = keepstale ]; then exit 0; fi
        rm -f "\$DEV/\$f" ;;
      "run-as dev.arl.audio cat "*)
        f="\${line##* }"
        cat "\$DEV/\$f" ;;
      "getprop sys.boot_completed") echo "\$(state boot)" ;;
      "getprop ro.build.version.sdk") echo "\$(state sdk)" ;;
      "pm path dev.arl.audio"|"pm path dev.arl.audio.test") echo "package:/fake.apk" ;;
      "pm list instrumentation")
        echo "instrumentation:dev.arl.audio.test/androidx.test.runner.AndroidJUnitRunner (target=dev.arl.audio)" ;;
      dumpsys*)
        r=false; n=false
        [ "\$(state record)" = granted ] && r=true
        [ "\$(state notif)" = granted ] && n=true
        echo "android.permission.RECORD_AUDIO: granted=\$r"
        echo "android.permission.POST_NOTIFICATIONS: granted=\$n" ;;
      "am force-stop "*) ;;
      pm\ grant*)
        perm="\${line##* }"
        if [ "\$perm" = android.permission.RECORD_AUDIO ]; then
          sed -i 's/^record=.*/record=granted/' "\$DEV/state"
        else
          sed -i 's/^notif=.*/notif=granted/' "\$DEV/state"
        fi ;;
      pm\ revoke*)
        perm="\${line##* }"
        if [ "\$perm" = android.permission.RECORD_AUDIO ]; then
          sed -i 's/^record=.*/record=denied/' "\$DEV/state"
        else
          sed -i 's/^notif=.*/notif=denied/' "\$DEV/state"
        fi ;;
      am\ instrument*)
        method="\$(printf '%s\n' \$line | sed -n 's/.*FgsLifecycleTest#//p' | awk '{print \$1}')"
        name="\$method"
        [ "\$MODE" = wrongname ] && name="otroMetodo"
        if [ "\$MODE" != noxml ]; then
          cat >"\$DEV/files/\${method}.xml" <<XML
<?xml version="1.0" encoding="UTF-8"?>
<testsuite name="dev.arl.audio.FgsLifecycleTest" tests="1" failures="0" errors="0" skipped="0">
  <testcase classname="dev.arl.audio.FgsLifecycleTest" name="\${name}" time="0.1"></testcase>
</testsuite>
XML
        fi
        exit "\$(state instrument_rc)" ;;
      *) echo "fake-adb: shell no simulado: \$line" >&2; exit 9 ;;
    esac ;;
  exec-out)
    line="\$*"
    case "\$line" in
      "run-as dev.arl.audio rm -f "*)
        f="\${line##* }"
        if [ "\$MODE" = keepstale ]; then exit 0; fi
        rm -f "\$DEV/\$f" ;;
      "run-as dev.arl.audio cat "*)
        f="\${line##* }"
        cat "\$DEV/\$f" ;;
      *) echo "fake-adb: exec-out no simulado: \$line" >&2; exit 9 ;;
    esac ;;
  *) echo "fake-adb: comando no simulado: \$cmd" >&2; exit 9 ;;
esac
EOF
  chmod +x "$BASE/fake-$mode"
}

expect_fail() {
  local mode="$1" needle="$2" rc="$3"
  local dev="$BASE/dev-$mode" home="$BASE/home-$mode" sdk="$BASE/sdk-$mode"
  mkdir -p "$dev/files" "$home/.cache/arl-emu" "$sdk/platform-tools"
  cat >"$dev/state" <<EOF
sdk=34
boot=1
record=denied
notif=denied
instrument_rc=$rc
EOF
  if [ "$mode" = keepstale ]; then
    printf '%s\n' '<testsuite tests="1" failures="0" errors="0" skipped="0"><testcase classname="dev.arl.audio.FgsLifecycleTest" name="viejo" time="0"/></testsuite>' \
      >"$dev/files/manifestDeclaresMicForegroundServiceAndNoLocalNetwork.xml"
  fi
  write_fake "$dev" "$mode"
  ln -sfn "$BASE/fake-$mode" "$sdk/platform-tools/adb"
  local out="$BASE/out-$mode" log="$BASE/$mode.log"
  set +e
  HOME="$home" ANDROID_HOME="$sdk" "$RUNNER" -s emulator-fake --out "$out" >"$log" 2>&1
  local got=$?
  set -e
  if [ "$got" -eq 0 ]; then
    echo "FALLO $mode: el runner salió 0" >&2
    exit 1
  fi
  if ! grep -q "$needle" "$log"; then
    echo "FALLO $mode: no aparece '$needle'" >&2
    cat "$log" >&2
    exit 1
  fi
  echo "ok $mode rechazado"
}

expect_fail keepstale "sigue presente tras borrarlo" 0
expect_fail noxml "no dejó XML" 0
expect_fail badexit "instrument exit=137" 137
expect_fail wrongname "se esperaba dev.arl.audio.FgsLifecycleTest#" 0
echo "NEGATIVOS_OK"
