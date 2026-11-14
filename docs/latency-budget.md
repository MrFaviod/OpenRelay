# Presupuesto de latencia y recursos —

documento fija objetivos y techos para poder medir después, y declara el método
de cada cifra. Ningún número de aquí está logrado, benchmarkeado ni observado en
un dispositivo. La prioridad del proyecto es la latencia en LAN; el entorno de
host observado es Linux aarch64 nativo (Rust 1.80.1), no un
teléfono.

## Presupuesto por etapa (propuesto)

Camino de extremo a extremo para frame v1 (PCM16LE mono 48 kHz, 5 ms por
frame). `budget_us` es el objetivo de diseño y `ceiling_us` el techo aceptable;
ambos son propuestas.

| Etapa | Objetivo | Techo | Método / nota |
|---|---:|---:|---|
| Captura | 5 ms | 12 ms | buffer del dispositivo de origen; no medido |
| Frame | 5 ms | 5 ms | duración fija del perfil v1; no es procesamiento medido |
| LAN (ida) | 3 ms | 15 ms | prioridad del proyecto; sin cifras de red medidas |
| Jitter | 10 ms | 40 ms | target por defecto; 40 ms es el máximo del contrato |
| Resample + salida | 2 ms | 8 ms | resample adaptativo y entrega al sink; no medido |
| **Total** | **25 ms** | **80 ms** | suma de etapas, no latencia observada |

La fila Jitter usa el target por defecto de `arl-audio-core` (10 ms) y su máximo
de contrato (40 ms); no es una espera medida. La fila LAN es la prioridad: si
una medición futura incumple, el orden de ataque es red → jitter → captura.

Bloque comprobado por `tools/verify.py --budget --phase f1`: el verificador
recalcula sumas y coherencia, no mide latencia.

<!-- verify:latency-budget -->
```json
{
  "status": "proposed_not_measured"
  "end_to_end_budget_us": 25000
  "end_to_end_ceiling_us": 80000
  "stages": [
    {"stage": "capture", "budget_us": 5000, "ceiling_us": 12000}
    {"stage": "frame", "budget_us": 5000, "ceiling_us": 5000}
    {"stage": "lan", "budget_us": 3000, "ceiling_us": 15000}
    {"stage": "jitter", "budget_us": 10000, "ceiling_us": 40000}
    {"stage": "resample_output", "budget_us": 2000, "ceiling_us": 8000}
  ]
}
```

## Contadores idle y streaming

`arl_audio_core::budget` separa dos acumuladores que nunca se mezclan:

- **Idle**: proceso vivo sin audio en curso. Cuenta sólo lo que el consumidor
  observe; los contadores de jitter y drift suelen quedar en cero.
- **Streaming**: audio en curso. Acumula por delta las métricas reales de
  `JitterBuffer` (frames emitidos, huecos, PLC, descartes) y de
  `DriftController` (resyncs). No reimplementa el pipeline ni crea un segundo
  contador de verdad.

`frames`, `samples`, `gaps`, `plc_frames`, `dropped`, `resyncs`, `underruns` y
`overruns` saturan en `u64::MAX`: un flujo hostil no desborda ni paniquea. Las
pruebas `budget_idle_stream` comprueban la separación de modos y que una segunda
lectura sin cambios no duplica cuentas.

## Techos de recursos (propuestos)

| Recurso | Techo | Unidad | Método de medida |
|---|---:|---|---|
| RSS idle | 24 MiB | bytes | `VmRSS` de `/proc/<pid>/status` en reposo |
| RSS streaming | 48 MiB | bytes | `VmRSS` de `/proc/<pid>/status` con audio |
| CPU idle | 10 ms/s | ms/s | `utime+stime` de `/proc/<pid>/stat` por intervalo |
| CPU streaming | 150 ms/s | ms/s | `utime+stime` de `/proc/<pid>/stat` por intervalo |
| Artefacto | 8 MiB | bytes | binario release desplegado, no `target/` |
| Hilos propios | 4 | hilos | por nombre/rol; ART/Compose/RenderThread aparte |
| Heap de control | 64 KiB | bytes | worker de control; no incluye audio preasignado |

Los techos son cotas de propuesta. Cualquier medición futura se compara con
`budget::check(recurso, observado)`; un exceso devuelve `Verdict::Over`, no un
PASS.

## Memoria

- **Audio preasignado**: 16 slots del jitter buffer (cota superior de 508 B por
  frame) más el frame de PLC. Se reserva una vez y no crece con el flujo.
- **Asignación transitoria en el camino de audio**: cero
  (`TRANSIENT_AUDIO_ALLOC_BYTES = 0`).
- **Heap del worker de control**: presupuesto separado y acotado a 64 KiB. No
  comparte cuenta con el audio preasignado.

## Hilos Android

Sólo los hilos propios de la app entran en el presupuesto: captura, red
consumidor de jitter y worker de control (4). **ART, Compose y RenderThread son
hilos externos del sistema**: se miden por separado con Perfetto o
`dumpsys`, y este presupuesto no los suma ni los presenta como propios.

## Qué no es este documento

- No es un benchmark ni una medición de latencia, CPU, memoria o red.
- La simulación de drift de 1 h (`drift_1h`, marcada `#[ignore]`) verifica el
  lazo con relojes simulados; no es una prueba de hardware ni de LAN.
- No elige dispositivo ni teléfono de referencia: la medición física queda
  pendiente y su referencia (modelo, SO, buffer de audio, punto de red) se
  registrará al medir. El host de compilación es sólo el entorno de compilación y
  test observado.
- No aporta cifras logradas ni convierte un criterio global en PASS. Las
  aportaciones quedan pendiente hasta la revisión independiente y la medición
  física.
