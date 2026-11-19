#!/usr/bin/env python3
"""Instrumenta SOLO backend_fixture.c: declara openrelay_test_hook y llama enable_done
tras fx->enabled=1, antes de volver de enable. No toca life.c ni include/ del motor.
Cada ancla debe aparecer exactamente una vez; si no, sale con error (SETUP).
Uso: instrument_fixture.py <ENGINE_DIR> <BUILD_DIR>"""
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


os.makedirs(out + "/tests", exist_ok=True)
shutil.copy(eng + "/tests/backend_fixture.h", out + "/tests/backend_fixture.h")
fx = rd(eng + "/tests/backend_fixture.c")
fx = sub(fx, '#include "backend_fixture.h"',
         '#include "backend_fixture.h"\n\nvoid openrelay_test_hook(const char *id);', "decl_fx")
fx = sub(fx, "  fx->enabled = 1;\n  fx->closed = 0;\n",
         '  fx->enabled = 1;\n  fx->closed = 0;\n  openrelay_test_hook("enable_done");\n',
         "enable_done")
with open(out + "/tests/backend_fixture.c", "w", encoding="utf-8") as f:
    f.write(fx)
print("INSTR_OK")
