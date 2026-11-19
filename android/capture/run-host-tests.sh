#!/bin/sh
# run-host-tests.sh — strict host runner for the T13a shared SPSC core.
#
# Rules this runner enforces, because a printed warning is not a gate:
#   * a header that differs from the frozen contract FAILS the run (pinned
#     sha256 is authoritative; a local baseline dir is additionally cmp'd);
#   * the 15 original test cases must be byte-identical to the reviewed ones;
#   * any ASan/UBSan/TSan report in a log FAILS the run even when the runtime
#     itself exits 0 — the log is scanned, not trusted to the exit code;
#   * S1, S2, S4 and S5 are positive controls; S3 is a regression.
#     The mandatory residual probe adds partial drain plus a further push
#     while payload remains across the wrap; other phases also keep payload;
#   * the scenarios and the residual probe are ASan/UBSan-instrumented too,
#     not only the 15 cases and the wrap repro;
#   * a missing timeout(1) does not authorize an unbounded command: a portable
#     watchdog kills the phase at the same budget;
#   * the first TSan startup abort is preserved in its own file before the
#     per-process setarch -R retry, and no global setting is touched.
# Installs nothing, writes only inside this tree, honours TMPDIR.
set -u

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 2
cd "$ROOT" || exit 2

BUILD="$ROOT/build"
LOGS="$ROOT/logs"
mkdir -p "$BUILD" "$LOGS" || exit 2

CC=${CC:-cc}
STD="-std=c11 -O2 -Wall -Wextra -Werror -pthread"
INC="-I$ROOT/include"
SRC="$ROOT/src/spsc.c"
TST="$ROOT/tests/capture_spsc.c"
WRAP="$ROOT/tests/capture_spsc_counter_wrap.c"
SCEN="$ROOT/tests/capture_spsc_counter_scenarios.c"
RES="$ROOT/tests/capture_spsc_residual.c"

# frozen contract, from the durable baseline
HASH_ABI=5bca6df782ba260955043952188d6b90d6297788a4f372c2d2557a4b066bcc77
HASH_SPSC=b7b270d062de60a9ce7829b24856fbeb08a2435397078cc3a6f0cae98b97cdca
# the 15 original cases as reviewed and confirmed by the parent
HASH_TESTS=d30faf43102806c435917ba7832ccda570286acfbddf403b20cd313c5186e75d

TMO=120          # compile / instrumented runs
TMO_WRAP=45      # release wrap regression (public fast-forward)
TMO_STRESS=15    # stress case inside the 15
LOG="$LOGS/host-tests.log"
: > "$LOG" || exit 2

FAILS=0
EXIT=0
GAPS=""

say() { printf '%s\n' "$*" | tee -a "$LOG"; }
note_gap() { GAPS="$GAPS$1; "; say "GAP: $1"; }
mark_fail() { FAILS=$((FAILS + 1)); if [ "$EXIT" -eq 0 ]; then EXIT=$1; fi; }

# run_limited <label> <seconds> <outfile> <cmd...>
# Uses timeout(1) when present; otherwise its own watchdog, so a missing tool
# never turns into an unbounded command.
run_limited() {
    _label=$1; _secs=$2; _out=$3; shift 3
    if command -v timeout >/dev/null 2>&1; then
        timeout --signal=TERM --kill-after=5 "$_secs" "$@" >"$_out" 2>&1
        _rc=$?
    else
        "$@" >"$_out" 2>&1 &
        _pid=$!
        _waited=0
        while kill -0 "$_pid" 2>/dev/null; do
            if [ "$_waited" -ge "$_secs" ]; then
                say "WATCHDOG ${_secs}s: $_label (no timeout(1) available)"
                kill -TERM "$_pid" 2>/dev/null
                sleep 2
                kill -KILL "$_pid" 2>/dev/null
                wait "$_pid" 2>/dev/null
                return 124
            fi
            sleep 1
            _waited=$((_waited + 1))
        done
        wait "$_pid"
        _rc=$?
    fi
    [ "$_rc" -eq 124 ] && say "TIMEOUT ${_secs}s: $_label"
    return "$_rc"
}

# any sanitizer / UB report in a log?
sanitizer_report() {
    grep -qE 'ERROR: (AddressSanitizer|LeakSanitizer)|SUMMARY: (AddressSanitizer|UndefinedBehaviorSanitizer)|WARNING: ThreadSanitizer|runtime error:' "$1" 2>/dev/null
}

# phase <label> <exit> <mandatory:yes|no> <logfile>
phase() {
    _l=$1; _rc=$2; _mand=$3; _log=$4
    if [ "$_rc" -eq 0 ]; then
        say "  $_l: exit=0"
    elif [ "$_rc" -eq 124 ]; then
        say "  $_l: exit=124 TIMEOUT"
        if [ "$_mand" = yes ]; then
            mark_fail 124
        else
            note_gap "$_l hit its ${TMO}s budget; not verified, not passed"
        fi
    else
        say "  $_l: exit=$_rc"
        if [ "$_mand" = yes ]; then
            mark_fail "$_rc"
        else
            note_gap "$_l exited $_rc (optional phase)"
        fi
    fi
    [ -f "$_log" ] && tail -12 "$_log" | sed 's/^/    | /' | tee -a "$LOG"
}

for f in "$SRC" "$TST" "$WRAP" "$SCEN" "$RES"; do
    if [ ! -f "$f" ]; then
        say "FATAL missing input: $f"
        exit 2
    fi
done

say "== openrelay T13a SPSC host runner (strict) =="
say "root=$ROOT"
say "cc=$($CC --version 2>/dev/null | head -1)"
say "flags=$STD $INC"
say "date_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# ---------------------------------------------------------------- headers
say ""
say "-- phase 1: frozen headers (fails on any difference) --"
BASE="${OPENRELAY_BASELINE:-$ROOT/contract-baseline}"
HDR_OK=1
for pair in "capture-abi.h:$HASH_ABI" "capture-spsc.h:$HASH_SPSC"; do
    h=${pair%:*}; want=${pair#*:}
    got=$(sha256sum "$ROOT/include/$h" 2>/dev/null | cut -d' ' -f1)
    if [ "$got" = "$want" ]; then
        say "  IDENTICAL $h sha256=$got"
    else
        say "  DIFFERS   $h"
        say "    want $want"
        say "    got  ${got:-<missing>}"
        HDR_OK=0
    fi
    if [ -d "$BASE" ] && [ -f "$BASE/$h" ]; then
        if cmp -s "$ROOT/include/$h" "$BASE/$h"; then
            say "  cmp       $h byte-identical to local baseline"
        else
            say "  cmp       $h DIFFERS from local baseline"
            HDR_OK=0
        fi
    fi
done
if [ "$HDR_OK" -ne 1 ]; then
    mark_fail 1
    say "  HEADER GATE FAILED (a diff is a failure, not a notice)"
else
    say "  header gate: PASS"
fi

# ------------------------------------------------- original 15 must be intact
say ""
say "-- phase 2: original 15 cases unchanged --"
GOT_T=$(sha256sum "$TST" | cut -d' ' -f1)
if [ "$GOT_T" = "$HASH_TESTS" ]; then
    say "  tests/capture_spsc.c sha256=$GOT_T (matches reviewed copy)"
else
    say "  tests/capture_spsc.c sha256=$GOT_T"
    say "  EXPECTED                    $HASH_TESTS"
    mark_fail 1
    say "  ORIGINAL-TESTS GATE FAILED"
fi
say "  core src/spsc.c sha256=$(sha256sum "$SRC" | cut -d' ' -f1)"

# ------------------------------------------------------- release: 15 cases
say ""
say "-- phase 3: release -O2, 15 original cases --"
run_limited "release-build" "$TMO" "$LOGS/build-release.log" \
    $CC $STD $INC \
    -Wl,--wrap=malloc -Wl,--wrap=calloc -Wl,--wrap=realloc -Wl,--wrap=free \
    -DOPENRELAY_WRAP_ALLOC -o "$BUILD/spsc-tests" "$SRC" "$TST"
BR=$?
phase "release-build" "$BR" yes "$LOGS/build-release.log"
if [ "$BR" -eq 0 ]; then
    run_limited "release-run" "$TMO" "$LOGS/run-release.log" \
        env OPENRELAY_STRESS_SCALE=1 "$BUILD/spsc-tests"
    RR=$?
    say "  release-run exit=$RR"
    grep -E '^\[(PASS|FAIL)\]|^summary:|^phase A' "$LOGS/run-release.log" | tee -a "$LOG"
    P=$(grep -c '^\[PASS\]' "$LOGS/run-release.log")
    F=$(grep -c '^\[FAIL\]' "$LOGS/run-release.log")
    say "  cases: pass=$P fail=$F"
    if [ "$RR" -ne 0 ] || [ "$P" -ne 15 ] || [ "$F" -ne 0 ]; then
        mark_fail "$([ "$RR" -ne 0 ] && echo "$RR" || echo 1)"
        say "  ORIGINAL-15 GATE FAILED (need exactly 15 PASS, 0 FAIL)"
    fi
else
    mark_fail "$BR"
fi

# ------------------------------------------- release: public wrap regression
say ""
say "-- phase 4: release wrap regression (public parent repro, mandatory) --"
run_limited "wrap-build" "$TMO" "$LOGS/build-wrap.log" \
    $CC $STD $INC -o "$BUILD/wrap-public" "$SRC" "$WRAP"
BW=$?
phase "wrap-build" "$BW" yes "$LOGS/build-wrap.log"
if [ "$BW" -eq 0 ]; then
    run_limited "wrap-run" "$TMO_WRAP" "$LOGS/run-wrap.log" "$BUILD/wrap-public"
    RW=$?
    say "  wrap-run exit=$RW"
    tail -6 "$LOGS/run-wrap.log" | sed 's/^/    | /' | tee -a "$LOG"
    MM=$(sed -n 's/.*mismatches=\([0-9]*\).*/\1/p' "$LOGS/run-wrap.log" | tail -1)
    say "  mismatches=${MM:-<none>}"
    if [ "$RW" -ne 0 ] || [ "${MM:-1}" != "0" ]; then
        mark_fail "$([ "$RW" -ne 0 ] && echo "$RW" || echo 1)"
        say "  WRAP GATE FAILED"
    else
        say "  wrap gate: PASS (0 mismatches across the 2^32 counter wrap)"
    fi
fi

# ----------------------------------------- release: counter-wrap scenarios
say ""
say "-- phase 5: release counter-wrap scenarios (drain/read/free/laps) --"
run_limited "scen-build" "$TMO" "$LOGS/build-scenarios.log" \
    $CC $STD $INC -o "$BUILD/wrap-scenarios" "$SRC" "$SCEN"
BS=$?
phase "scen-build" "$BS" yes "$LOGS/build-scenarios.log"
if [ "$BS" -eq 0 ]; then
    run_limited "scen-run" "$TMO" "$LOGS/run-scenarios.log" "$BUILD/wrap-scenarios"
    RS=$?
    say "  scen-run exit=$RS"
    grep -E '^\[(PASS|FAIL)\]|^scenarios:' "$LOGS/run-scenarios.log" | tee -a "$LOG"
    SP=$(grep -c '^\[PASS\]' "$LOGS/run-scenarios.log")
    SF=$(grep -c '^\[FAIL\]' "$LOGS/run-scenarios.log")
    say "  scenarios: pass=$SP fail=$SF"
    if [ "$RS" -ne 0 ] || [ "$SP" -ne 5 ] || [ "$SF" -ne 0 ]; then
        mark_fail "$([ "$RS" -ne 0 ] && echo "$RS" || echo 1)"
        say "  SCENARIO GATE FAILED"
    fi
fi

# ------------------------------------ release: residual occupancy across wrap
# Four of the five scenarios are positive controls; S3 fails on the old core.
# This mandatory phase adds a partial drain followed by another push while
# retaining payload across 2^32. Other phases also test occupied crossings.
say ""
say "-- phase 5b: release residual occupancy across the wrap (mandatory) --"
run_limited "residual-build" "$TMO" "$LOGS/build-residual.log" \
    $CC $STD $INC -o "$BUILD/wrap-residual" "$SRC" "$RES"
BRES=$?
phase "residual-build" "$BRES" yes "$LOGS/build-residual.log"
if [ "$BRES" -eq 0 ]; then
    run_limited "residual-run" "$TMO" "$LOGS/run-residual.log" "$BUILD/wrap-residual"
    RRES=$?
    say "  residual-run exit=$RRES"
    grep -E '^(  (s3|residual|drain_residual)|s3 rc=|residual rc=|drain_residual rc=)' \
        "$LOGS/run-residual.log" | tee -a "$LOG"
    RC_S3=$(sed -n 's/^s3 rc=\([0-9]*\).*/\1/p' "$LOGS/run-residual.log" | tail -1)
    RC_RES=$(sed -n 's/^residual rc=\([0-9]*\).*/\1/p' "$LOGS/run-residual.log" | tail -1)
    RC_DRN=$(sed -n 's/^drain_residual rc=\([0-9]*\).*/\1/p' "$LOGS/run-residual.log" | tail -1)
    S3_WIN=$(sed -n 's/.*s3 mismatches_windows=\([0-9]*\).*/\1/p' "$LOGS/run-residual.log" | tail -1)
    RES_MIS=$(sed -n 's/.*residual mis_a=\([0-9]*\) mis_b=\([0-9]*\).*/\1 \2/p' "$LOGS/run-residual.log" | tail -1)
    DRN_MIS=$(sed -n 's/.*drain_residual mismatches=\([0-9]*\).*/\1/p' "$LOGS/run-residual.log" | tail -1)
    say "  residual rc: s3=${RC_S3:-<none>} residual=${RC_RES:-<none>} drain=${RC_DRN:-<none>}"
    say "  residual counts: s3_windows=${S3_WIN:-<none>} mis_a/mis_b=${RES_MIS:-<none>} drain=${DRN_MIS:-<none>}"
    if [ "$RRES" -ne 0 ] || [ "${RC_S3:-1}" != "0" ] || [ "${RC_RES:-1}" != "0" ] || \
        [ "${RC_DRN:-1}" != "0" ]; then
        mark_fail "$([ "$RRES" -ne 0 ] && echo "$RRES" || echo 1)"
        say "  RESIDUAL GATE FAILED (need exit 0 and three rc=0 lines)"
    elif [ "${S3_WIN:-1}" != "0" ] || [ "${RES_MIS:-1}" != "0 0" ] || [ "${DRN_MIS:-1}" != "0" ]; then
        mark_fail 1
        say "  RESIDUAL GATE FAILED (rc=0 but a mismatch count is not zero)"
    else
        say "  residual gate: PASS (payload FIFO intact across the 2^32 wrap)"
    fi
fi

# asan_case <label> <test-source> <binary-name> [expected PASS count]
# One instrumented build+run under the same strict options as phase 6. A
# report in the log fails the run even when the runtime exits 0; a timeout is
# a declared gap, never a pass.
asan_case() {
    # NOTE: phase()/run_limited() assign the globals _l, _log, _rc, _out, so the
    # names here must not collide with them or the label is clobbered mid-call.
    _acl=$1; _acts=$2; _acbn=$3; _acwp=${4:-}
    run_limited "$_acl-build" "$TMO" "$LOGS/build-$_acl.log" \
        $CC $STD -g -O1 -fno-omit-frame-pointer -fsanitize=address,undefined \
        $INC -o "$BUILD/$_acbn" "$SRC" "$_acts"
    _acb=$?
    phase "$_acl-build" "$_acb" no "$LOGS/build-$_acl.log"
    if [ "$_acb" -ne 0 ]; then
        note_gap "$_acl: instrumented build exited $_acb; not verified, not passed"
        return
    fi
    ASAN_OPTIONS="halt_on_error=1:abort_on_error=0:exitcode=1:detect_leaks=1:print_summary=1" \
    UBSAN_OPTIONS="halt_on_error=1:exitcode=1:print_stacktrace=1" \
        run_limited "$_acl-run" "$TMO" "$LOGS/run-$_acl.log" "$BUILD/$_acbn"
    _acr=$?
    say "  $_acl-run exit=$_acr"
    grep -E '^\[(PASS|FAIL)\]|^scenarios:|^s3 rc=|^residual rc=|^drain_residual rc=' \
        "$LOGS/run-$_acl.log" | tee -a "$LOG"
    if sanitizer_report "$LOGS/run-$_acl.log"; then
        say "  SANITIZER REPORT PRESENT in run-$_acl.log (failing regardless of runtime exit)"
        grep -nE 'ERROR: (AddressSanitizer|LeakSanitizer)|SUMMARY:|runtime error:' "$LOGS/run-$_acl.log" \
            | head -8 | tee -a "$LOG"
        mark_fail 1
    elif [ "$_acr" -eq 124 ]; then
        note_gap "$_acl-run hit the ${TMO}s budget; not verified"
    elif [ "$_acr" -ne 0 ]; then
        mark_fail "$_acr"
    elif [ -n "$_acwp" ]; then
        _acp=$(grep -c '^\[PASS\]' "$LOGS/run-$_acl.log")
        _acf=$(grep -c '^\[FAIL\]' "$LOGS/run-$_acl.log")
        say "  $_acl cases: pass=$_acp fail=$_acf (want $_acwp)"
        if [ "$_acp" -ne "$_acwp" ] || [ "$_acf" -ne 0 ]; then
            mark_fail 1
            say "  $_acl INSTRUMENTED GATE FAILED (need $_acwp PASS, 0 FAIL)"
        fi
    fi
}

# --------------------------------------------------- strict ASan/UBSan
# halt_on_error + exitcode make the runtime itself fail on the first report;
# the log scan below is a second, independent gate because some runtimes still
# exit 0 with sanitizer_common's default options.
say ""
say "-- phase 6: strict ASan+UBSan (targeted: 15 cases + wrap) --"
run_limited "asan-build" "$TMO" "$LOGS/build-asan.log" \
    $CC $STD -g -O1 -fno-omit-frame-pointer -fsanitize=address,undefined \
    $INC -o "$BUILD/spsc-asan" "$SRC" "$TST"
BA=$?
phase "asan-build" "$BA" no "$LOGS/build-asan.log"
if [ "$BA" -eq 0 ]; then
    ASAN_OPTIONS="halt_on_error=1:abort_on_error=0:exitcode=1:detect_leaks=1:print_summary=1" \
    UBSAN_OPTIONS="halt_on_error=1:exitcode=1:print_stacktrace=1" \
        run_limited "asan-run" "$TMO" "$LOGS/run-asan.log" "$BUILD/spsc-asan"
    RA=$?
    say "  asan-run exit=$RA"
    grep -E '^\[(PASS|FAIL)\]|^summary:' "$LOGS/run-asan.log" | tee -a "$LOG"
    if sanitizer_report "$LOGS/run-asan.log"; then
        say "  SANITIZER REPORT PRESENT in run-asan.log (failing regardless of runtime exit)"
        grep -nE 'ERROR: (AddressSanitizer|LeakSanitizer)|SUMMARY:|runtime error:' "$LOGS/run-asan.log" | head -8 | tee -a "$LOG"
        mark_fail 1
    elif [ "$RA" -ne 0 ]; then
        if [ "$RA" -eq 124 ]; then
            note_gap "asan-run hit the ${TMO}s budget; not verified"
        else
            mark_fail "$RA"
        fi
    fi

    # wrap repro under the same strict instrumentation
    run_limited "asan-wrap-build" "$TMO" "$LOGS/build-asan-wrap.log" \
        $CC $STD -g -O1 -fno-omit-frame-pointer -fsanitize=address,undefined \
        $INC -o "$BUILD/wrap-asan" "$SRC" "$WRAP"
    BAW=$?
    phase "asan-wrap-build" "$BAW" no "$LOGS/build-asan-wrap.log"
    if [ "$BAW" -eq 0 ]; then
        ASAN_OPTIONS="halt_on_error=1:abort_on_error=0:exitcode=1:detect_leaks=1:print_summary=1" \
        UBSAN_OPTIONS="halt_on_error=1:exitcode=1:print_stacktrace=1" \
            run_limited "asan-wrap-run" "$TMO" "$LOGS/run-asan-wrap.log" "$BUILD/wrap-asan"
        RAW=$?
        say "  asan-wrap-run exit=$RAW"
        tail -3 "$LOGS/run-asan-wrap.log" | sed 's/^/    | /' | tee -a "$LOG"
        if sanitizer_report "$LOGS/run-asan-wrap.log"; then
            say "  SANITIZER REPORT PRESENT in run-asan-wrap.log"
            mark_fail 1
        elif [ "$RAW" -eq 124 ]; then
            note_gap "asan-wrap-run hit the ${TMO}s budget; wrap not sanitizer-verified"
        elif [ "$RAW" -ne 0 ]; then
            mark_fail "$RAW"
        fi
    fi
else
    note_gap "ASan/UBSan unavailable here (build exit=$BA); not passed"
fi

# ------------------------------------- strict ASan/UBSan: scenarios + residual
# The five scenarios and the residual probe were outside the sanitizer phases
# before this cut; a failure in S3 had no instrumented run at all.
say ""
say "-- phase 6b: strict ASan+UBSan (counter scenarios + residual) --"
asan_case "asan-scenarios" "$SCEN" "scen-asan" 5
asan_case "asan-residual" "$RES" "residual-asan"
# The residual probe has no [PASS] lines: its contract is the three rc= lines.
if [ -f "$LOGS/run-asan-residual.log" ] && ! sanitizer_report "$LOGS/run-asan-residual.log"; then
    AR3=$(sed -n 's/^s3 rc=\([0-9]*\).*/\1/p' "$LOGS/run-asan-residual.log" | tail -1)
    ARR=$(sed -n 's/^residual rc=\([0-9]*\).*/\1/p' "$LOGS/run-asan-residual.log" | tail -1)
    ARD=$(sed -n 's/^drain_residual rc=\([0-9]*\).*/\1/p' "$LOGS/run-asan-residual.log" | tail -1)
    say "  asan-residual rc: s3=${AR3:-<none>} residual=${ARR:-<none>} drain=${ARD:-<none>}"
    if [ "${AR3:-1}" != "0" ] || [ "${ARR:-1}" != "0" ] || [ "${ARD:-1}" != "0" ]; then
        mark_fail 1
        say "  ASAN-RESIDUAL GATE FAILED (instrumented run did not return three rc=0)"
    fi
fi

# ------------------------------------------------------------- TSan
say ""
say "-- phase 7: TSan over the 15 cases (stress included) --"
run_limited "tsan-build" "$TMO" "$LOGS/build-tsan.log" \
    $CC $STD -g -O1 -fsanitize=thread $INC -o "$BUILD/spsc-tsan" "$SRC" "$TST"
BT=$?
if [ "$BT" -eq 0 ]; then
    TSAN_OPTIONS="halt_on_error=1:exitcode=1:second_deadlock_stack=1" \
        run_limited "tsan-run" "$TMO" "$LOGS/run-tsan.log" "$BUILD/spsc-tsan"
    RT=$?
    say "  tsan-run exit=$RT"
    # preserve the very first abort before any retry can overwrite it
    if [ "$RT" -ne 0 ] && grep -q 'unexpected memory mapping' "$LOGS/run-tsan.log"; then
        cp "$LOGS/run-tsan.log" "$LOGS/run-tsan-first-abort.log"
        say "  first abort preserved in logs/run-tsan-first-abort.log"
        if command -v setarch >/dev/null 2>&1; then
            say "  retrying per-process with setarch -R (no global ASLR change)"
            TSAN_OPTIONS="halt_on_error=1:exitcode=1" \
                run_limited "tsan-run-setarch" "$TMO" "$LOGS/run-tsan-setarch.log" \
                setarch "$(uname -m)" -R "$BUILD/spsc-tsan"
            RT=$?
            say "  tsan-run(setarch -R) exit=$RT"
            TLOG="$LOGS/run-tsan-setarch.log"
        else
            TLOG="$LOGS/run-tsan-first-abort.log"
        fi
    else
        TLOG="$LOGS/run-tsan.log"
    fi
    grep -E '^\[(PASS|FAIL)\]|^summary:' "$TLOG" 2>/dev/null | tee -a "$LOG"
    if sanitizer_report "$TLOG"; then
        say "  TSAN REPORT PRESENT in $TLOG"
        grep -nE 'WARNING: ThreadSanitizer' "$TLOG" | head -5 | tee -a "$LOG"
        mark_fail 1
    elif [ "$RT" -ne 0 ]; then
        if [ "$RT" -eq 124 ]; then
            note_gap "tsan-run hit the ${TMO}s budget; not verified"
        else
            note_gap "tsan did not complete here (exit=$RT, no race report); not passed"
        fi
    else
        say "  tsan gate: PASS"
    fi
    say "  note: TSan covered the 15 cases including stress; the 2^32 fast-forward"
    say "        repro was NOT tsan-instrumented (its budget is the release run)."
else
    note_gap "TSan unavailable here (build exit=$BT); not passed"
fi

# ---------------------------------------------------------------- summary
say ""
say "-- summary --"
say "failures=$FAILS"
[ -n "$GAPS" ] && say "gaps: $GAPS"
if [ "$FAILS" -ne 0 ]; then
    say "RESULT=FAIL"
elif [ -n "$GAPS" ]; then
    say "RESULT=PASS_WITH_DECLARED_GAPS"
else
    say "RESULT=PASS"
fi
say "EXIT=$EXIT"
exit "$EXIT"
