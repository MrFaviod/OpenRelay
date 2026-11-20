# Códec Opus

**Estado: implementado a nivel de crate, no integrado en la sesión.** La app
Android todavía no negocia Opus; el flujo real es PCM16 y así lo declaran el
README y la matriz de compatibilidad. El camino PCM16 fijo de sigue siendo
el contrato por defecto y no se modifica. Este documento describe
`openrelay-codec`, sus perfiles y sus límites; no afirma calidad por escucha ni
latencia extremo a extremo medida.

## Perfiles

Todos los perfiles usan `opus::Application::LowDelay`
(`OPUS_APPLICATION_RESTRICTED_LOWDELAY`): CELT únicamente, mono, 48 kHz, PCM16
como entrada y salida.

| Perfil | Bloque | Muestras | Paquete a 64 kbps | Paquete a 96 kbps | Latencia algorítmica (bloque + lookahead) |
|---|---:|---:|---:|---:|---:|
| `LowDelay2p5Ms` | 2.5 ms | 120 | ~20 B | ~30 B | 5.0 ms |
| `LowDelay5Ms` | 5 ms | 240 | ~40 B | ~60 B | 7.5 ms |
| `LowDelay10Ms` | 10 ms | 480 | ~80 B | ~120 B | 12.5 ms |

- Bitrate aceptado: **64–96 kbps mono**; fuera de ese rango
  `OpusEncoder::new` devuelve `InvalidBitrate`.
- El bitrate es un objetivo: libopus usa VBR por defecto, así que el tamaño real
  varía por trama. El tope duro es `MAX_PACKET_LEN` = 1200 − 28 = **1172 bytes**
  (datagrama del transporte menos la cabecera del frame v1).
- La latencia algorítmica es bloque + lookahead; **no es latencia extremo a
  extremo**. El lookahead del modo restricted-lowdelay es 2.5 ms (120 muestras)
  y está cubierto por `lookahead_is_2_5_ms`.

## Comparación con PCM16 fijo

PCM16 mono 48 kHz ocupa 2 bytes por muestra: **480 bytes por trama de 5 ms =
768 kbps** de payload, más 28 bytes de cabecera por datagrama. Opus no cambia
el contrato de frame: si se adopta, su paquete sustituye al payload PCM16 dentro
del mismo presupuesto de datagrama.

| Camino | Payload 5 ms | Bitrate de payload | Datagrama con cabecera |
|---|---:|---:|---:|
| PCM16 fijo | 480 B | 768 kbps | 508 B |
| Opus 5 ms a 64 kbps | ~40 B | 64 kbps | ~68 B |
| Opus 5 ms a 96 kbps | ~60 B | 96 kbps | ~88 B |

La reducción de ancho de banda es de aproximadamente 8–12× en 5 ms. No se
declara transparencia ni “mejor calidad”: Opus es un códec con pérdida y la
comparación de calidad requiere escucha, que este entregable no incluye.

## Pérdida de paquetes: PLC, no FEC

- `OpusDecoder::decode(None)` (o `Some(&)`) decodifica sin paquete y libopus
  aplica **PLC** con la última duración de paquete conocida. La salida mide
  siempre exactamente una trama del perfil.
- El PLC depende del estado del decodificador: hay que decodificar el flujo en
  orden y no reiniciar entre pérdidas. Una pérdida larga degrada la señal
  oculta; el PLC no recupera el contenido perdido.
- **FEC en banda: no disponible y no prometida.** En libopus la FEC pertenece a
  SILK (y modos híbridos); `OPUS_APPLICATION_RESTRICTED_LOWDELAY` usa sólo CELT.
  Este crate nunca llama a `set_inband_fec` y siempre decodifica con
  `fec = false`. No se debe configurar el receptor esperando FEC.

## Entrada a 44.1 kHz

libopus sólo acepta 8, 12, 16, 24 o 48 kHz; 44.1 kHz no es una tasa válida de
entrada. Una fuente a 44.1 kHz debe remuestrearse a 48 kHz **antes** del
codificador:

- `openrelay_audio_core::LinearResampler` es un corrector de drift: su ratio está
  clampado a `[0.98, 1.02]`, por lo que no sirve para 44.1 → 48 (ratio 1.088).
- El remuestreo 44.1 → 48 corresponde a la etapa de captura/adaptación previa
  fuera de este crate. `OpusEncoder::encode` rechaza cualquier buffer que no
  tenga exactamente las muestras del perfil a 48 kHz (`InvalidPcmLength`).

## Fallback y rollback a PCM16

El códec es **opcional**:

- El frame v1 y el pipeline PCM16 no dependen de `openrelay-codec`; el crate no toca
  `openrelay-audio-core`, el transporte ni la sesión.
- La elección de perfil y el anuncio de que se usa Opus pertenecen a la capa de
  sesión/negociación (fuera del alcance de este crate). En v1 no hay
  autodetección: el receptor debe saber si el payload es PCM16 u Opus.
- **Rollback**: si el perfil no se negocia, el códec falla o la calidad no es
  aceptable, el emisor vuelve a emitir PCM16 fijo de. El receptor conserva
  el camino PCM16 como fallback; no se requiere convertir datos ya enviados.
- Un `OpusEncoder`/`OpusDecoder` nuevo por sesión y por dirección; no son
  `Sync`, así que cada estado se usa desde un solo hilo a la vez.

## API

- `CodecProfile::{LowDelay2p5Ms, LowDelay5Ms, LowDelay10Ms}` con
  `samples_per_frame`, `frame_duration_us`, `from_frame_samples` y `ALL`.
- `OpusEncoder::new(profile, bitrate_bps)`, `encode(&[i16]) -> Vec<u8>`
  `lookahead_samples`, `reset`.
- `OpusDecoder::new(profile)`, `decode(Option<&[u8]>) -> Vec<i16>`, `reset`.
- Errores en `CodecError` (sin `thiserror`): tamaños y bitrates inválidos
  longitudes PCM incorrectas, paquete demasiado grande y errores de libopus.

## Dependencias y licencias

| Componente | Versión | Licencia | Nota |
|---|---|---|---|
| `opus` (crate) | =0.3.0 | MIT OR Apache-2.0 | bindings seguros |
| `audiopus_sys` | 0.2.2 | ISC | FFI y build de libopus |
| `libopus` (bundled) | según `audiopus_sys` | BSD-3-Clause | compilado con CMake |

El build requiere CMake y un compilador de C. Nuestro crate mantiene
`unsafe_code = "forbid"`; el `unsafe` vive sólo en las dependencias FFI.

## Verificación

```sh
cargo test --locked -p openrelay-codec --test codec_profiles
cargo clippy -p openrelay-codec -- -D warnings
cargo fmt -p openrelay-codec -- --check
```

`codec_profiles` cubre: roundtrip en 2.5/5/10 ms y bitrates 64/80/96 kbps
rechazo de tamaños y bitrates inválidos, PLC con longitud correcta, paquete
dentro de 1200 − 28 bytes, SNR/energía frente a un seno y lookahead de 2.5 ms.
Pasar estas pruebas no demuestra calidad perceptual, latencia E2E ni
interoperabilidad de red.
