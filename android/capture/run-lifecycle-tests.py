#!/usr/bin/env python3
"""run-lifecycle-tests.py - gate portable fail-closed de la integracion lifecycle host T13a (final).

Uso: python3 run-lifecycle-tests.py [--out DIR]

--out DIR: el gate crea un subdirectorio unico (tempfile.mkdtemp) dentro de DIR;
nunca borra nada existente. Rechaza DIR dentro del paquete o ancestro del paquete.

- Rutas relativas al directorio de este script; sin rutas absolutas ni del workspace.
- Binarios y logs van a DIR (por defecto <padre del paquete>/arl-t13a-final-gate-logs), fuera del arbol fuente.
- Argumentos de procesos como listas (sin shell): soporta rutas con espacios.
- Exit 0 solo si TODOS los checks pasan. Build fallido, exit distinto del esperado,
  timeout, reporte de sanitizer, marcador ausente, FAIL/RED/SETUP o hash distinto => exit 1.
- Nunca usa run-all.sh ni agrega rc sin verificar marcadores.
"""
import argparse
import hashlib
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
PKG = pathlib.Path(__file__).resolve().parent

# Hashes congelados de entradas contractuales/base (no se reescriben).
FROZEN = {
    "src/spsc.c": "13547c283257db6715f631df0cb0eb550a4a5aa8b80d066a0cfe47bee2155ec4",
    "include/capture-abi.h": "298f88c637bcf39f17690e6d61d265bd50eab75a576c0f035ae8d521dd8d96e4",
    "include/capture-backend.h": "567a328713ea8453479eb39e7eec3d95aa0d761635871dca965248b56be910f6",
    "include/capture-spsc.h": "b6a2a5dd954a6bd32da6c98bc10325bc7b2afc5345b7255403793ba9a4823d4c",
    "include/capture-owner.h": "3bac96cf815cf7091bf0c1026f63ebbc3f8dbe493bd4f1b9dc60ef83f9762645",
    "src/life.c": "4b2217d54b5e49f5d635e9a40b19dced893edff470f10ebeb26b9ee812993988",
    "tests/lifecycle-base/src/life.c": "e3077eb6a8364c2f3cd62e19d7630eed19c30a864d456304489c5f04249de196",
    "tests/fixture/backend_fixture.c": "f3ca7af058a47a10fad959c173063cfcbf2c9c2eb36fb93a5adc97428b6ac1d8",
    "tests/fixture/backend_fixture.h": "5e6b565ac5d5c0edd4127ff87ad7c8b21d8213810bf7a20dbfc3c0725abe3064",
}

SAN_RE = re.compile(r"ERROR: (AddressSanitizer|LeakSanitizer|ThreadSanitizer)|runtime error|SUMMARY: ")
FAIL_RE = re.compile(r"RESULT=RED|RESULT=FAIL|^FAIL |SETUP_FAIL", re.M)

INC = ["-Iinclude", "-Itests/fixture", "-Itests/owner"]
WARN = ["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread"] + INC
SAN = ["-std=c11", "-O1", "-g", "-Wall", "-Wextra", "-Werror", "-pthread",
       "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-fno-omit-frame-pointer"] + INC
WRAP = ["-Wl,--wrap=arl_spsc_read", "-Wl,--wrap=arl_spsc_drain"]
FIX = "tests/fixture/backend_fixture.c"
CAND_CORE = ["src/life.c", "src/spsc.c", "src/capture-owner.c", FIX]
BASE_CORE = ["tests/lifecycle-base/src/life.c", "src/spsc.c", FIX]
OWNED_LIB = ["src/life.c", "src/spsc.c", "src/capture-owner.c", "src/capture-owned.c", FIX]

LONG_TO = 60
INF_TO = 15
BUILD_TO = 60

RESULTS = []
OUT = None
LOGS = None
BIN = None
ENVV = {"LC_ALL": "C",
        "ASAN_OPTIONS": "halt_on_error=1:abort_on_error=1:detect_leaks=1",
        "UBSAN_OPTIONS": "halt_on_error=1:print_stacktrace=1"}


def step(name, ok, detail=""):
    RESULTS.append((name, ok, detail))
    print(("OK   " if ok else "FAIL ") + name + (" :: " + detail if detail else ""), flush=True)


def sh(argv, log, cwd, timeout):
    """Ejecuta argv (lista, sin shell). Devuelve rc; 124 si timeout; 127 si no ejecutable."""
    env = dict(os.environ)
    env.update(ENVV)
    with open(log, "w", encoding="utf-8", errors="replace") as f:
        f.write("$ " + " ".join(argv) + "\n")
        f.flush()
        try:
            p = subprocess.run(argv, cwd=str(cwd), stdout=f, stderr=subprocess.STDOUT,
                               timeout=timeout, env=env)
            return p.returncode
        except subprocess.TimeoutExpired:
            f.write("TIMEOUT\n")
            return 124
        except OSError as e:
            f.write(f"OSERROR {e}\n")
            return 127


def sha256_file(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def safe_rel(rel):
    p = pathlib.PurePosixPath(rel)
    return (rel != "" and "\\" not in rel and not p.is_absolute()
            and all(part not in ("", ".", "..") for part in p.parts))


def verify_manifest():
    man = PKG / "lifecycle-manifest.sha256"
    if not man.is_file():
        step("manifest presente", False, "falta lifecycle-manifest.sha256")
        return
    listed = {}
    bad_fmt = []
    for n, line in enumerate(man.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        parts = line.split("  ", 1)
        if len(parts) != 2 or not re.fullmatch(r"[0-9a-f]{64}", parts[0]) or not safe_rel(parts[1]):
            bad_fmt.append(f"linea{n}")
            continue
        if parts[1] in listed:
            bad_fmt.append(f"dup:{parts[1]}")
            continue
        listed[parts[1]] = parts[0]
    step("manifest formato", not bad_fmt, ";".join(bad_fmt))
    bad = [r for r, d in listed.items() if not (PKG / r).is_file() or sha256_file(PKG / r) != d]
    step("manifest entradas listadas coinciden", bool(listed) and not bad,
         f"entries={len(listed)} mismatch={bad}")
    for rel, digest in FROZEN.items():
        ok = (PKG / rel).is_file() and sha256_file(PKG / rel) == digest
        step(f"hash congelado {rel}", ok)


def build(name, argv):
    rc = sh(argv, LOGS / f"build-{name}.log", PKG, BUILD_TO)
    step(f"build {name}", rc == 0, f"rc={rc}")
    return rc == 0


def run_case(name, binpath, args, timeout, markers):
    log = LOGS / f"run-{name}.log"
    rc = sh([str(binpath)] + list(args), log, OUT, timeout)
    text = log.read_text(encoding="utf-8", errors="replace")
    problems = []
    if rc != 0:
        problems.append(f"exit={rc}")
    if SAN_RE.search(text):
        problems.append("reporte de sanitizer")
    if FAIL_RE.search(text):
        problems.append("FAIL/RED/SETUP en log")
    for m in markers:
        if m not in text:
            problems.append(f"falta marcador '{m}'")
    step(f"run {name}", not problems, "; ".join(problems) if problems else "ok")
    return not problems


def quiescence(tag, lib, san_owner):
    """Suite de quiescencia: public, backend, inflight read/drain/control (normal y san) y owner_smoke si aplica."""
    variants = [("", WARN), ("_san", SAN)]
    built = {}
    for suf, fl in variants:
        built["public" + suf] = build(f"{tag}-public{suf}",
            ["cc"] + fl + ["tests/public/capture_life_public.c"] + lib + ["-o", str(BIN / f"{tag}_public{suf}")])
        built["backend" + suf] = build(f"{tag}-backend{suf}",
            ["cc"] + fl + ["tests/backend/capture_life_backend.c"] + lib + ["-o", str(BIN / f"{tag}_backend{suf}")])
        built["inflight" + suf] = build(f"{tag}-inflight{suf}",
            ["cc"] + fl + WRAP + ["tests/inflight/inflight_test.c"] + lib + ["-o", str(BIN / f"{tag}_inflight{suf}")])
        if san_owner:
            built["owner" + suf] = build(f"{tag}-owner{suf}",
                ["cc"] + fl + ["tests/owner/owner_smoke.c"] + lib + ["-o", str(BIN / f"{tag}_owner{suf}")])
    LIFE_OK = ["LIFECYCLE_CASES_EXECUTED=6", "BACKEND_CONTRACT_CASES_EXECUTED=1", "180 checks, 0 failures"]
    INF_READ = ["WRAP_REACHED op=read consumer_inside_public_api=1", "DESTROY_WHILE_HELD rc=-5 owner_after=",
                "POST_RELEASE_DESTROY rc=0 owner_after_null=1", "CONSUMER_API_CALLED=arl_capture_read rc=0 n=240",
                "RESULT=PASS destroy BUSY while consumer in flight"]
    INF_DRAIN = ["WRAP_REACHED op=drain consumer_inside_public_api=1", "DESTROY_WHILE_HELD rc=-5 owner_after=",
                 "POST_RELEASE_DESTROY rc=0 owner_after_null=1", "CONSUMER_API_CALLED=arl_capture_drain rc=0 n=240",
                 "RESULT=PASS destroy BUSY while consumer in flight"]
    INF_CTRL = ["CONTROL_DESTROY rc=0 owner_after_null=1", "RESULT=PASS control destroy OK when quiescent"]
    OWN = ["OWNER_SMOKE_CASES=10 fails=0", "RESULT=PASS owner smoke"]
    # (nombre, clave de build, argumentos, timeout, marcadores)
    cases = []
    for suf in ["", "_san"]:
        cases.append((f"public{suf}", "public" + suf, [], LONG_TO, ["95 checks, 0 failures"]))
        cases.append((f"backend{suf}", "backend" + suf, [], LONG_TO, LIFE_OK))
    for suf in ["", "_san"]:
        cases.append((f"inflight_read{suf}", "inflight" + suf, ["read"], INF_TO, INF_READ))
        cases.append((f"inflight_drain{suf}", "inflight" + suf, ["drain"], INF_TO, INF_DRAIN))
        cases.append((f"inflight_control{suf}", "inflight" + suf, ["control"], INF_TO, INF_CTRL))
        if san_owner:
            cases.append((f"owner{suf}", "owner" + suf, [], 30, OWN))
    ok = True
    for name, bkey, args, to, markers in cases:
        full = f"{tag}-{name}"
        if not built.get(bkey, False):
            step(f"run {full}", False, f"build {bkey} fallido")
            ok = False
            continue
        ok = run_case(full, BIN / f"{tag}_{bkey}", args, to, markers) and ok
    return ok


def admission_instrument(src_life, dst, forced):
    text = src_life.read_text(encoding="utf-8")

    def sub(t, anchor, repl):
        n = t.count(anchor)
        if n != 1:
            raise SystemExit(f"ANCLA count={n} (want 1): {anchor.strip()[:60]}")
        return t.replace(anchor, repl)

    decl = ("void arl_test_before_adm_cas(arl_capture *cap, uint64_t unit);\n"
            "void arl_test_getter_window(arl_capture *cap);\n")
    text = sub(text, "static int adm_enter(arl_capture *cap, uint64_t unit) {\n",
               decl + "static int adm_enter(arl_capture *cap, uint64_t unit) {\n")
    mode = "NOT_REQUESTED"
    if forced:
        cas = "    if (atomic_compare_exchange_strong(&cap->adm, &old, old + unit)) {\n"
        if text.count(cas) == 1:
            text = text.replace(cas, "    arl_test_before_adm_cas(cap, unit);\n" + cas)
            mode = "FORCED"
        else:
            mode = "UNFORCED_NO_CAS_ANCHOR"
        text = sub(text,
                   "  if (!adm_enter(cap, ADM_OTHER)) {\n    return ARL_CAPTURE_ERR_BUSY; /* *out no se escribe */\n  }\n",
                   "  if (!adm_enter(cap, ADM_OTHER)) {\n    return ARL_CAPTURE_ERR_BUSY; /* *out no se escribe */\n  }\n"
                   "  arl_test_getter_window(cap);\n")
    dst.write_text(text, encoding="utf-8")
    return mode


def admission_suite():
    ok = True
    build_dir = OUT / "admission" / "build"
    build_dir.mkdir(parents=True, exist_ok=True)
    try:
        mode = admission_instrument(PKG / "src/life.c", build_dir / "life_instr.c", True)
        step("instrumentacion admision", True, f"mode={mode}")
    except SystemExit as e:
        step("instrumentacion admision", False, str(e))
        return False
    if mode != "UNFORCED_NO_CAS_ANCHOR":
        step("modo admision esperado", False, f"mode={mode}")
        return False
    OTH = ["src/spsc.c", "src/capture-owner.c", FIX]
    if not build("admission-fixture", ["cc"] + WARN + ["-c", FIX, "-o", str(build_dir / "fixture.o")]):
        return False
    b1 = build("admission-normal", ["cc"] + WARN + ["tests/admission/admission_regression.c", str(build_dir / "life_instr.c")] + OTH + ["-o", str(BIN / "admission")])
    b2 = build("admission-san", ["cc"] + SAN + ["tests/admission/admission_regression.c", str(build_dir / "life_instr.c")] + OTH + ["-o", str(BIN / "admission_san")])
    if not (b1 and b2):
        return False
    markers = ["ok P1 forced7 push OK", "ok N1 stale token => ERR_CLOSED", "ADMISSION_CASES checks=18 fails=0",
               "RESULT=PASS admission regression"]
    ok = run_case("admission-normal", BIN / "admission", [], LONG_TO, markers) and ok
    ok = run_case("admission-san", BIN / "admission_san", [], LONG_TO, markers) and ok
    return ok


def view_engine():
    """Copia de solo lectura del paquete en la forma que esperan los instrumentadores (include/, src/, tests/)."""
    view = OUT / "view"
    if view.exists():
        shutil.rmtree(view)
    (view / "tests").mkdir(parents=True)
    shutil.copytree(PKG / "include", view / "include")
    shutil.copytree(PKG / "src", view / "src")
    shutil.copy(PKG / "tests/fixture/backend_fixture.c", view / "tests/backend_fixture.c")
    shutil.copy(PKG / "tests/fixture/backend_fixture.h", view / "tests/backend_fixture.h")
    return view


def events_suite():
    view = view_engine()
    eng = OUT / "events" / "build" / "engine"
    shutil.rmtree(OUT / "events" / "build", ignore_errors=True)
    eng.parent.mkdir(parents=True)
    rc = sh([sys.executable, "tests/events/instrument.py", str(view), str(eng)],
            LOGS / "instrument-events.log", PKG, BUILD_TO)
    if rc != 0 or "INSTR_OK" not in (LOGS / "instrument-events.log").read_text():
        step("instrumentacion events", False, f"rc={rc}")
        return False
    step("instrumentacion events", True)
    ENG_SRC = [str(eng / "src/life.c"), str(eng / "src/spsc.c"), str(eng / "tests/backend_fixture.c")]
    INC_E = ["-I" + str(eng / "include"), "-I" + str(eng / "tests")]
    W = ["cc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread"] + INC_E
    S = ["cc", "-std=c11", "-O1", "-g", "-Wall", "-Wextra", "-Werror", "-pthread",
         "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-fno-omit-frame-pointer"] + INC_E
    ok = build("events-normal", W + ["tests/events/test_events.c"] + ENG_SRC + ["-o", str(BIN / "events_normal")])
    ok = build("events-san", S + ["tests/events/test_events.c"] + ENG_SRC + ["-o", str(BIN / "events_san")]) and ok
    if not ok:
        return False
    res = True
    for v, b in [("normal", "events_normal"), ("san", "events_san")]:
        for c in ["counters_retained", "stale_token", "disconnect_control", "stop_window",
                  "enable_callback", "effective_mismatch"]:
            res = run_case(f"events-{v}-{c}", BIN / b, [c], 15, ["RESULT=PASS"]) and res
    return res


def enable_suite():
    view = view_engine()
    bld = OUT / "enable" / "build"
    shutil.rmtree(bld, ignore_errors=True)
    (bld / "tests").mkdir(parents=True)
    rc = sh([sys.executable, "tests/enable/instrument_fixture.py", str(view), str(bld)],
            LOGS / "instrument-enable.log", PKG, BUILD_TO)
    if rc != 0 or "INSTR_OK" not in (LOGS / "instrument-enable.log").read_text():
        step("instrumentacion enable", False, f"rc={rc}")
        return False
    step("instrumentacion enable", True)
    SRC = ["src/life.c", "src/spsc.c", str(bld / "tests/backend_fixture.c")]
    INC_B = ["-Iinclude", "-I" + str(bld / "tests")]
    W = ["cc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror", "-pthread"] + INC_B
    S = ["cc", "-std=c11", "-O1", "-g", "-Wall", "-Wextra", "-Werror", "-pthread",
         "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-fno-omit-frame-pointer"] + INC_B
    ok = build("enable-normal", W + ["tests/enable/test_enable_events.c"] + SRC + ["-o", str(BIN / "enable_normal")])
    ok = build("enable-san", S + ["tests/enable/test_enable_events.c"] + SRC + ["-o", str(BIN / "enable_san")]) and ok
    if not ok:
        return False
    res = True
    for v, b in [("normal", "enable_normal"), ("san", "enable_san")]:
        for c in ["enable_error", "enable_disconnect", "control_positive"]:
            res = run_case(f"enable-{v}-{c}", BIN / b, [c], 15, ["RESULT=PASS"]) and res
    return res


def owner_suite():
    ok = True
    # Controles del core base (test-only, no es implementacion de producto).
    cc_lib = BASE_CORE
    cc_ok = build("core-controls", ["cc"] + WARN + ["tests/owner/core_controls.c"] + cc_lib +
                  ["-o", str(BIN / "core_controls")])
    if cc_ok:
        for m in ["quiescent", "retire_reopen"]:
            ok = run_case(f"core-controls-{m}", BIN / "core_controls", [m], LONG_TO,
                          ["RESULT=PASS"]) and ok
    else:
        ok = False
    ok = quiescence("base", BASE_CORE, False) and ok
    for f in ["tests/owner/owner_tests.c", "tests/owner/core_controls.c"]:
        r = sh(["cc"] + WARN + ["-fsyntax-only", f], LOGS / f"syntax-{pathlib.Path(f).name}.log", PKG, BUILD_TO)
        step(f"sintaxis {f}", r == 0, f"rc={r}")
        ok = ok and r == 0
    ENG = ["src/capture-owner.c", "src/life.c", "src/spsc.c", FIX]
    ob = build("owner-tests", ["cc"] + WARN + ["-c", "tests/owner/owner_tests.c", "-o", str(BIN / "owner_tests.o")])
    ob = build("owner-link", ["cc"] + WARN + ENG + [str(BIN / "owner_tests.o"), "-o", str(BIN / "owner_tests")] + WRAP) and ob
    os_ = build("owner-san", ["cc"] + SAN + ["tests/owner/owner_tests.c"] + ENG + ["-o", str(BIN / "owner_tests_san")] + WRAP)
    if not (ob and os_):
        return False
    for bname in ["owner_tests", "owner_tests_san"]:
        for c in ["1", "2read", "2drain", "3", "4", "5", "6", "7"]:
            ok = run_case(f"{bname}-{c}", BIN / bname, [c], 30, ["RESULT=PASS"]) and ok
    return ok


def owned_suite():
    ok = True
    ok = build("owned-normal", ["cc"] + WARN + ["tests/owned/owned_smoke.c"] + OWNED_LIB + ["-o", str(BIN / "owned_smoke")]) and ok
    ok = build("owned-san", ["cc"] + SAN + ["tests/owned/owned_smoke.c"] + OWNED_LIB + ["-o", str(BIN / "owned_smoke_san")]) and ok
    if not ok:
        return False
    M = ["RESULT=PASS owned smoke", "OWNED_SMOKE_CASES=19 fails=0"]
    ok = run_case("owned-normal", BIN / "owned_smoke", [], 30, M)
    ok = run_case("owned-san", BIN / "owned_smoke_san", [], 30, M) and ok
    return ok


def final_window_suite():
    """Oraculos T13a final-window (mismos oraculos; candidato life.c congelado, WINDOW_WANT=0)."""
    bld = OUT / "final-window" / "build"
    bld.mkdir(parents=True)
    rc = sh([sys.executable, "tests/final-window/instrument.py", str(PKG / "src/life.c"), str(bld / "life_instr.c")],
            LOGS / "instrument-final-window.log", PKG, BUILD_TO)
    text = (LOGS / "instrument-final-window.log").read_text(encoding="utf-8", errors="replace")
    if rc != 0 or "INSTRUMENT_OK inserted=3" not in text:
        step("instrumentacion final-window", False, f"rc={rc}")
        return False
    step("instrumentacion final-window", True)
    SRC = ["src/capture-owner.c", str(bld / "life_instr.c"), "src/spsc.c", FIX, "tests/final-window/t13a_final_window.c"]
    ok = build("final-window-normal", ["cc"] + WARN + SRC + ["-o", str(BIN / "final_window")])
    ok = build("final-window-san", ["cc"] + SAN + SRC + ["-o", str(BIN / "final_window_san")]) and ok
    if not ok:
        return False
    res = True
    for v, b in [("normal", "final_window"), ("san", "final_window_san")]:
        res = run_case(f"final-window-{v}-window", BIN / b, ["window"], 15,
                       ["RESULT=PASS window"]) and res
        res = run_case(f"final-window-{v}-reject", BIN / b, ["reject"], 15,
                       ["RESULT=PASS reject"]) and res
    return res


def main():
    global OUT, LOGS, BIN
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    base = pathlib.Path(a.out).resolve() if a.out else (PKG.parent / "arl-t13a-lifecycle-gate-logs").resolve()
    if PKG == base or PKG in base.parents or base in PKG.parents:
        print("FAIL: --out dentro del paquete o ancestro del paquete (rechazado, nada borrado)", file=sys.stderr)
        return 1
    if base.exists() and not base.is_dir():
        print("FAIL: --out existe y no es directorio (nada borrado)", file=sys.stderr)
        return 1
    base.mkdir(parents=True, exist_ok=True)
    OUT = pathlib.Path(tempfile.mkdtemp(prefix="run-", dir=str(base))).resolve()
    LOGS = OUT / "logs"
    BIN = OUT / "bin"
    LOGS.mkdir(parents=True)
    BIN.mkdir(parents=True)
    print("OUT " + str(OUT), flush=True)
    for k, v in ENVV.items():
        os.environ[k] = v

    verify_manifest()
    # Matriz completa; cada suite registra sus checks y continua para reportar todo.
    quiescence_ok = quiescence("cand", CAND_CORE, True)
    step("suite quiescence candidata", quiescence_ok)
    step("suite owner base + owner tests", owner_suite())
    step("suite owned smoke (19 checks)", owned_suite())
    step("suite events (12 casos)", events_suite())
    step("suite enable-events (6 casos)", enable_suite())
    step("suite admision R1 (PASS, modo sin ancla CAS)", admission_suite())
    step("suite final-window (window PASS, reject PASS)", final_window_suite())

    failed = [n for n, ok, _ in RESULTS if not ok]
    summary = [f"{'OK  ' if ok else 'FAIL'} {n} {d}" for n, ok, d in RESULTS]
    summary.append(f"TOTAL={len(RESULTS)} FAILED={len(failed)}")
    summary.append("GATE " + ("PASS" if not failed else "FAIL"))
    (OUT / "RESULT.txt").write_text("\n".join(summary) + "\n", encoding="utf-8")
    print("\n".join(summary[-2:]))
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
