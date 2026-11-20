# Latency targets (not measured)

**Status: proposed targets, not measurements.** There is no physical baseline
yet: this document sets targets and ceilings so they can be measured later, and
declares the method for each number. No number here has been achieved
benchmarked, or observed on a device. The project's priority is LAN latency.

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

La fila Jitter usa el target por defecto de `openrelay-audio-core` (10 ms) y su máximo
de contrato (40 ms); no es una espera medida. La fila LAN es la prioridad: si
una medición futura incumple, el orden de ataque es red → jitter → captura.

El bloque siguiente es la suma de etapas del contrato; no mide latencia.

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

## Medición

El presupuesto de arriba sigue siendo **propuesta, no medición**. La medición
física requiere un dispositivo de referencia, una captura con `pw-record` y la
evidencia guardada con hash; hasta entonces no hay resultados.

- **Latencia extremo a extremo:** p50/p95/p99 por detección de clics y
  correlación cruzada entre referencia y captura `pw-record`. La resolución
  del análisis es de 1 muestra (0,0208 ms a 48 kHz); la incertidumbre física
  se declara aparte.
- **RTT:** se mide y reporta en su propio campo `rtt_ms`, aparte de
  `latency_ms`. No se suma, resta ni promedia con la latencia; confundirlos
  invalida la evidencia.
- **Drift de 1 h:** `cargo test -p openrelay-audio-core --test drift_1h -- --ignored`
  es la referencia simulada; la evidencia de hardware exige un stream real de
  60 min con `ppm` y `resyncs`.
- **Evidencia:** cada captura se guarda con su hash y el entorno (modelo, SO
  buffer de audio y punto de red). Sin captura física no hay cifra.

Estado actual: **sin medición física**. No hay baseline físico ni dispositivo de referencia
medido; estas cifras no se presentan como logradas. El README no publica cifras de
latencia: sólo este documento las menciona, y como objetivo. Cuando exista evidencia
válida se actualizará el bloque `verify:latency-budget`; hasta entonces conserva
`proposed_not_measured` para no declarar mediciones inexistentes.
## Contadores idle y streaming

`openrelay_audio_core::budget` separa dos acumuladores que nunca se mezclan:

- **Idle**: proceso vivo sin audio en curso. Cuenta sólo lo que el consumidor
  observe mientras el modo está en idle.
- **Streaming**: audio en curso. Acumula por delta las métricas reales de
  `JitterBuffer` (frames emitidos de red, muestras, huecos, PLC, descartes) y de
  `DriftController` (resyncs). No reimplementa el pipeline ni crea un segundo
  contador de verdad.

La línea base del pipeline es **una sola y compartida**: cada `observe`
acumula el delta en el modo vigente y avanza la línea base del otro modo sin
sumarle nada (`rebase`). Así cada delta se cuenta exactamente una vez y
alternar `streaming → idle → streaming` no reatribuye emisiones ya contadas: si
el buffer emitió 1, 2 y 3 frames y se observó en streaming, idle y streaming
los acumuladores dan 2 en streaming y 1 en idle (suma 3), no 5.

Casos límite declarados:

- **Observación repetida**: la misma instantánea sin cambios da delta cero en
  todos los campos; no duplica el pipeline.
- **Cambio de modo entre observaciones**: el intervalo no observado se atribuye
  al modo vigente en la siguiente observación, porque no hay muestra intermedia
  que lo divida. Para atribuir el límite con precisión, observa antes de cambiar
  de modo.
- **Saturación**: `frames`, `samples`, `gaps`, `plc_frames`, `dropped`
  `resyncs`, `underruns` y `overruns` saturan en `u64::MAX`; un flujo hostil no
  desborda ni paniquea.
- **Reinicio del pipeline**: si los contadores leídos retroceden (buffer o
  controlador nuevos), el delta se cuenta desde cero en vez de recortarse a
  cero; el reinicio no pierde lo nuevo ni resta lo acumulado. Un reinicio que
  vuelve a un valor igual o mayor que la línea base no es distinguible sólo con
  contadores y se declara como límite del método.

Las pruebas `budget_idle_stream` cubren separación de modos, alternancia
streaming→idle→streaming, observación repetida, reinicio y saturación.

## Techos de recursos (propuestos)

| Recurso | Techo | Unidad | Método de medida |
|---|---:|---|---|
| RSS idle | 24 MiB | bytes | `VmRSS` de `/proc/<pid>/status` en reposo |
| RSS streaming | 48 MiB | bytes | `VmRSS` de `/proc/<pid>/status` con audio |
| CPU idle | 10 ms/s | ms/s | `utime+stime` de `/proc/<pid>/stat` por intervalo |
| CPU streaming | 150 ms/s | ms/s | `utime+stime` de `/proc/<pid>/stat` por intervalo |
| Artefacto | 8 MiB | bytes | binario release desplegado, no `target/` |
| Hilos propios | 4 | hilos | por nombre/rol; ART/Compose/RenderThread aparte |
| Heap de control | 64 KiB | bytes | hilo de control; no incluye audio preasignado |

Los techos son cotas de propuesta. Cualquier medición futura se compara con
`budget::check(recurso, observado)`; un exceso devuelve `Verdict::Over`, no una
aprobación.

## Memoria

Las cifras de memoria son `size_of` en compilación del estado inline, no el
tamaño wire del frame (508 B) ni RSS. Un `AudioFrame` en memoria Rust mide 492 B
y cada slot añade el tag de `Option` (512 B por slot), así que multiplicar 508 B
por slot no describe la memoria real.

| Estado inline | Bytes (host de compilación) | Qué cuenta |
|---|---:|---|
| `JitterBuffer` | 8872 | 16 slots con frame, último frame para PLC y contabilidad; es `AUDIO_PREALLOCATED_BYTES` |
| `LinearResampler` | 1048 | anillo de 488 muestras y fase entre frames |
| `DriftController` | 144 | configuración, ratio, integral y contadores |
| **Camino de audio** | **10064** | `AUDIO_PATH_STATE_BYTES` = suma de los tres |

- **Audio preasignado**: `AUDIO_PREALLOCATED_BYTES = size_of::<JitterBuffer>`
  (8872 B). Se reserva una vez y no crece con el flujo.
- **Estado del resampler y del controlador**: se declaran aparte
  (`RESAMPLER_STATE_BYTES`, `DRIFT_STATE_BYTES`) y sólo entran en el total
  `AUDIO_PATH_STATE_BYTES`; no se cuentan dentro del jitter buffer.
- **Asignación transitoria en el camino de audio**: cero
  (`TRANSIENT_AUDIO_ALLOC_BYTES = 0`).
- **Heap del hilo de control**: presupuesto separado y acotado a 64 KiB. No
  comparte cuenta con el audio preasignado.
- **No contado**: heap y metadatos del allocador, pilas de hilos, buffers de
  sistema/Android y RSS. Esas cifras se miden aparte con `VmRSS` y no forman
  parte de estas constantes.

Los valores de la tabla son los del host de compilación; las
constantes se derivan con `size_of` y el test falla si dejan de coincidir.

## Hilos Android

Sólo los hilos propios de la app entran en el presupuesto: captura, red
consumidor de jitter y hilo de control (4). **ART, Compose y RenderThread son
hilos externos del sistema**: se miden por separado con Perfetto o
`dumpsys`, y este presupuesto no los suma ni los presenta como propios.

## Qué no es este documento

- No es un benchmark ni una medición de latencia, CPU, memoria o red.
- La simulación de drift de 1 h (`drift_1h`, marcada `#[ignore]`) verifica el
  lazo con relojes simulados; no es una prueba de hardware ni de LAN.
- No elige dispositivo ni teléfono de referencia: la medición física queda
  pendiente y su referencia (modelo, SO, buffer de audio, punto de red) se
  registrará al medir. El host de compilación es sólo el entorno donde se
  construye y se ejecutan las pruebas.
- No aporta cifras logradas ni convierte un criterio global en aprobación. Las
  cifras quedan como objetivos hasta la revisión independiente y la medición
  física.
