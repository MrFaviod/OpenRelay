#!/usr/bin/env python3
"""Copia instrumentada de src/life.c con dos hooks de inyeccion de test (solo copia de test).

Uso: instrument.py <life.c fuente> <salida life_instr.c>
Inserta tres lineas en anclas unicas (si no hay exactamente una, sale con 3):
  1. Tras atomic_store(&cap->gate_token, tok) en openrelay_capture_start: pausa el control
     entre gate abierto y set_state(STARTED).
  2. Declaraciones de ambos hooks tras el include del SPSC.
  3. Rama de token rechazado en openrelay_capture_push, con adm ya admitido (ADM_PUSH
     pendiente) antes de adm_leave.
No cambia ninguna otra linea.
"""
import sys

src_path, out_path = sys.argv[1], sys.argv[2]
src = open(src_path, encoding="utf-8").read()

decl_anchor = '#include "capture-spsc.h"\n'
gate_anchor = "  atomic_store(&cap->gate_token, tok);\n"
reject_anchor = (
    "  if (generation == 0u || atomic_load(&cap->gate_token) != generation) {\n"
    "    adm_leave(cap, ADM_PUSH);\n"
)
# Variante candidata (life-final-candidate): el rechazo tambien mira el estado.
reject_anchor_v2 = (
    "  if (generation == 0u || atomic_load(&cap->gate_token) != generation ||\n"
    "      state_of(cap) != OPENRELAY_CAPTURE_STATE_STARTED) {\n"
    "    adm_leave(cap, ADM_PUSH);\n"
)

for name, anchor in (("decl", decl_anchor), ("gate", gate_anchor)):
    n = src.count(anchor)
    if n != 1:
        print(f"ANCHOR_MISSING {name} count={n}", file=sys.stderr)
        sys.exit(3)
n_v1, n_v2 = src.count(reject_anchor), src.count(reject_anchor_v2)
if n_v1 + n_v2 != 1:
    print(f"ANCHOR_MISSING reject v1={n_v1} v2={n_v2}", file=sys.stderr)
    sys.exit(3)
if n_v2 == 1:
    reject_anchor = reject_anchor_v2

decl = (
    decl_anchor
    + "/* final-window hooks: solo en la copia instrumentada de test. */\n"
    + "void openrelay_t13a_hook_start_gate_open(openrelay_capture *cap, uint64_t tok);\n"
    + "void openrelay_t13a_hook_push_reject(openrelay_capture *cap, uint64_t generation);\n"
)
out = src.replace(decl_anchor, decl, 1)
out = out.replace(
    gate_anchor, gate_anchor + "  openrelay_t13a_hook_start_gate_open(cap, tok);\n", 1)
out = out.replace(
    reject_anchor,
    reject_anchor.replace("    adm_leave(cap, ADM_PUSH);\n",
                          "    openrelay_t13a_hook_push_reject(cap, generation);\n    adm_leave(cap, ADM_PUSH);\n"),
    1,
)
open(out_path, "w", encoding="utf-8").write(out)
print("INSTRUMENT_OK inserted=3")
