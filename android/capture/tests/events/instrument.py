#!/usr/bin/env python3
"""Copia instrumentada del motor: sustituciones exactas. Cada ancla debe aparecer exactamente una vez.
Solo inserta llamadas arl_test_hook(id); no cambia logica. No escribe en ENGINE_DIR.
Uso: instrument.py <ENGINE_DIR> <BUILD_DIR>"""
import os
import shutil
import sys

eng, out = sys.argv[1], sys.argv[2]


def rd(p):
    with open(p, encoding="utf-8") as f:
        return f.read()


def sub(text, anchor, repl, label):
    n = text.count(anchor)
    if n != 1:
        sys.exit(f"SETUP_FAIL anchor {label} count={n}")
    return text.replace(anchor, repl)


os.makedirs(out + "/src", exist_ok=True)
os.makedirs(out + "/tests", exist_ok=True)
shutil.copytree(eng + "/include", out + "/include", dirs_exist_ok=True)
shutil.copy(eng + "/src/spsc.c", out + "/src/spsc.c")
shutil.copy(eng + "/tests/backend_fixture.h", out + "/tests/backend_fixture.h")

life = rd(eng + "/src/life.c")
life = sub(life, '#include "capture-spsc.h"\n',
           '#include "capture-spsc.h"\n\nvoid arl_test_hook(const char *id);\n', "decl_life")
life = sub(life, "  out->samples_written = atomic_load(&cap->c_written);",
           '  arl_test_hook("counters_admitted");\n  out->samples_written = atomic_load(&cap->c_written);',
           "counters")
life = sub(life, "  rc = arl_spsc_push(cap->ring, samples, n, &res);",
           '  arl_test_hook("push_gate_ok");\n  rc = arl_spsc_push(cap->ring, samples, n, &res);', "push")
with open(out + "/src/life.c", "w", encoding="utf-8") as f:
    f.write(life)

fx = rd(eng + "/tests/backend_fixture.c")
fx = sub(fx, '#include "backend_fixture.h"',
         '#include "backend_fixture.h"\n\nvoid arl_test_hook(const char *id);', "decl_fx")
fx = sub(fx, "  fx->enabled = 1;\n  fx->closed = 0;\n",
         '  fx->enabled = 1;\n  fx->closed = 0;\n  arl_test_hook("enable_done");\n', "enable")
with open(out + "/tests/backend_fixture.c", "w", encoding="utf-8") as f:
    f.write(fx)
print("INSTR_OK")
