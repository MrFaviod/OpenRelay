# Protocolo `arl` v1 — control y audio


Alcance: una sesión autorizada Android → Linux. El audio viaja en datagramas no fiables; el control viaja en un stream fiable. **No se define códec, handshake TLS, descubrimiento ni API administrativa remota.**

## 1. Términos

| Término | Significado |
|---|---|
| Daemon | Proceso Linux, autoridad única de configuración y revisión. |
| Cliente | Android (sesión remota) o CLI local (Unix socket). |
| Sesión | Conexión autenticada y autorizada del cliente al endpoint `session`. |
| Enrolamiento | Ventana local y endpoint `enroll`; no transporta audio ni control operativo. |
| `request_id` | Identificador de operación para ACK, error e idempotencia. |
| `revision` | Contador monotónico de configuración del daemon. |
| Época | Generación de un flujo de audio; cambia al reiniciar o reconfigurar el camino de audio. |

## 2. Canales

| Canal | Transporte | Fiabilidad | Contenido |
|---|---|---|---|
| Control | Stream fiable (QUIC stream en el candidato) | Entrega ordenada; errores explícitos | Mensajes JSON de este documento |
| Audio | Datagramas (QUIC DATAGRAM en el candidato) | Sin retransmisión; descarte permitido | Un frame PCM16 por datagrama |

- El audio **nunca** se envía por el stream de control ni el control por datagramas de audio.
- El control **no** se usa para transportar audio ni para «recuperar» frames perdidos.
- El endpoint `enroll` sólo habla mensajes de enrolamiento; audio, control y tickets se rechazan allí (ver [`security/pairing.md`](security/pairing.md)).

## 3. Framing de control

Cada mensaje de control ocupa exactamente un frame del stream:

```text
+----------------+----------------------------------------+
| u32 BE length | length bytes de JSON UTF-8 |
+----------------+----------------------------------------+
```

| Campo | Tipo | Límite |
|---|---|---|
| `length` | entero sin signo, big-endian | `1..=16384` |
| payload | JSON UTF-8 | hasta 16384 bytes |
| frame total | — | `4 + length` ≤ 16388 bytes |

Reglas de codificación:

- UTF-8 sin BOM; sin compresión; sin concatenación de mensajes en un frame.
- Sin claves duplicadas. Profundidad máxima de JSON: **8**. Máximo **64** claves por objeto y **64** elementos por array. Strings de hasta **1024** caracteres.
- Enteros exactos en el rango `0..=9007199254740991` (2^53−1); no se usan números en punto flotante para revisiones, épocas, secuencias ni longitudes.
- Un frame con `length` fuera de rango, JSON inválido, tipo desconocido o campo obligatorio ausente se rechaza con `invalid`; si el framing está corrupto, se cierra la conexión.
- **v1 es estricto:** los objetos no admiten propiedades adicionales. Una extensión incompatible requiere `arl.control/v2` o un campo opcional declarado en una revisión posterior del esquema.

## 4. Envelope de control

Todo mensaje es un objeto con esta forma (esquema: [`contracts/control-v1.schema.json`](../contracts/control-v1.schema.json)):

```json
{
  "schema_version": "arl.control/v1"
  "type": "config.set"
  "request_id": "8f14e45fceea167a"
  "expected_revision": 7
  "payload": { "key": "audio.gain_db", "value": 3.0 }
}
```

| Campo | Tipo | Obligatorio | Límite / regla |
|---|---|---|---|
| `schema_version` | string | sí | constante `arl.control/v1` |
| `type` | string | sí | uno de los tipos de §5 |
| `request_id` | string | peticiones y respuestas | `^[A-Za-z0-9_-]{1,64}$`; único por cliente y sesión |
| `expected_revision` | entero | sólo `config.set` | `0..=2^53−1`; debe igualar la revisión actual |
| `revision` | entero | `ack`, `capabilities`, `event` | `0..=2^53−1`; revisión resultante o actual |
| `payload` | objeto | sí | según el tipo; máx. 64 claves |

## 5. Mensajes v1

| Tipo | Dirección | `request_id` | `revision` | `payload` |
|---|---|---|---|---|
| `hello` | cliente → daemon | sí | no | `{role, version, protocol_version}` |
| `capabilities` | ambas | sí | sí | objeto de capacidades (§7) |
| `config.get` | cliente → daemon | sí | no | `{key: string \| null}` |
| `config.set` | cliente → daemon | sí | no (`expected_revision` sí) | `{key, value}` |
| `ack` | daemon → cliente | sí (eco) | sí | `{applied, requested, effective}` |
| `error` | daemon → cliente | sí (eco) | no | `{code, message, retryable, current_revision?}` |
| `event` | daemon → cliente | no | sí | `{name, data}` |
| `ping` / `pong` | ambas | sí | no | `{}` (máx. 0 claves) |

### 5.1 `hello`

```json
{ "schema_version": "arl.control/v1", "type": "hello", "request_id": "h1"
  "payload": { "role": "android", "version": "0.1.0", "protocol_version": "arl.control/v1" } }
```

`role` ∈ `android`, `linux-cli`, `linux-daemon`. El daemon responde `capabilities`. Una versión de protocolo distinta se rechaza con `version_unsupported`.

### 5.2 `config.get`

`payload.key` es una clave concreta o `null` para el snapshot completo. La respuesta es un `ack` con `applied: true`, `requested: null` y `effective` con la clave o el snapshot completo. `config.get` no altera la revisión.

### 5.3 `config.set`

```json
{ "schema_version": "arl.control/v1", "type": "config.set", "request_id": "c-17"
  "expected_revision": 7
  "payload": { "key": "audio.gain_db", "value": 3.0 } }
```

Reglas:

1. `expected_revision` debe ser igual a la revisión actual del daemon. Si no, se devuelve `conflict` con `current_revision` y **no se aplica nada**.
2. La clave debe existir y ser de alcance `shared`. Una clave `local` o `readonly` se rechaza con `local_key` / `denied` y no se aplica.
3. El valor se valida contra tipo y rango de [`config-schema.md`](config-schema.md). Un valor inválido se rechaza con `invalid` y no se aplica.
4. Si el valor efectivo coincide con el actual, se devuelve `ack` con `applied: false` y la revisión no cambia.
5. Si se aplica, la revisión avanza exactamente **+1** y el `ack` devuelve `requested` y `effective`. `effective` puede diferir de `requested` por límites del dispositivo o del SO; el cliente debe mostrar ambos.
6. El daemon emite un `event` `config.changed` con la revisión nueva y `data` `{key, requested, effective}` (valores, no pares) a los clientes suscritos.

### 5.4 `ack`

```json
{ "schema_version": "arl.control/v1", "type": "ack", "request_id": "c-17", "revision": 8
  "payload": { "applied": true
               "requested": { "key": "audio.gain_db", "value": 3.0 }
               "effective": { "key": "audio.gain_db", "value": 3.0 } } }
```

`requested` es `null` en respuestas a `config.get` y en operaciones sin payload de cambio. `effective` nunca es `null`.

### 5.5 `error`

```json
{ "schema_version": "arl.control/v1", "type": "error", "request_id": "c-17"
  "payload": { "code": "conflict", "message": "revision mismatch", "retryable": true
               "current_revision": 9 } }
```

`message` es texto corto (≤200 caracteres), sin secretos ni rutas internas. Catálogo completo en [`contracts/errors.md`](../contracts/errors.md).

### 5.6 `event`

Eventos del daemon → cliente. `payload.name` es `config.changed` o `session.state`; cada nombre fija una forma **cerrada** de `data`, sin claves adicionales y sin objetos ni arrays anidados. `data` no contiene audio ni secretos. Un evento no requiere ACK ni `request_id`. La reconexión usa snapshot + eventos; no hay replay indefinido.

`config.changed` se emite tras un `config.set` aplicado, con la revisión nueva. `data.key` es la clave; `requested` y `effective` son los valores solicitado y efectivo, con las mismas restricciones por clave que `config.set` (pueden diferir por límites del dispositivo o del SO).

```json
{ "schema_version": "arl.control/v1", "type": "event", "revision": 8
  "payload": { "name": "config.changed"
               "data": { "key": "audio.gain_db", "requested": 3.0, "effective": 3.0 } } }
```

`session.state` informa el cambio de estado de la sesión: `active` al autorizar y `closed` al cerrar. `detail` es opcional, texto corto (≤200) sin secretos ni rutas internas.

```json
{ "schema_version": "arl.control/v1", "type": "event", "revision": 8
  "payload": { "name": "session.state"
               "data": { "state": "closed", "detail": "client closed" } } }
```

| Evento | Campos obligatorios | Campos opcionales | Límites |
|---|---|---|---|
| `config.changed` | `key`, `requested`, `effective` | — | `key` 1..=64 con el patrón de §4; `requested`/`effective` según [`config-schema.md`](config-schema.md) |
| `session.state` | `state` | `detail` | `state` ∈ `active`, `closed`; `detail` ≤ 200 caracteres |

## 6. Revisiones e idempotencia

- La revisión es un entero `0..=2^53−1`, propiedad exclusiva del worker de control del daemon.
- Empieza en `0` al arrancar el proceso; **no se promete monotonicidad entre reinicios**. Tras reconectar, el cliente obtiene el snapshot y no asume nada de su revisión anterior.
- Sólo un `config.set` aplicado incrementa la revisión; `config.get`, `ping`, `event` y los rechazos no la cambian.
- **Idempotencia:** el daemon guarda la última respuesta (`ack` o `error`) por `request_id` en una caché acotada por sesión:
  - máximo **64** entradas, evicción FIFO;
  - TTL **60 s**;
  - mismo `request_id` + mismo payload canónico ⇒ se devuelve la respuesta guardada y **no se reaplica**;
  - mismo `request_id` + payload distinto ⇒ `error` con `request_id_reuse` (no retryable).
- La caché se pierde al cerrar la sesión; el cliente debe reenviar con `expected_revision` o releer el snapshot. No hay idempotencia global entre sesiones.
- El orden de mensajes dentro del stream es el orden de aplicación. El daemon no aplica dos `config.set` en paralelo: el worker de control es único.

## 7. Capacidades

`capabilities` describe qué puede pedir cada extremo y con qué límites (esquema: [`contracts/capabilities-v1.schema.json`](../contracts/capabilities-v1.schema.json)):

```json
{
  "schema_version": "arl.capabilities/v1"
  "device_id": "a1b2c3d4e5f60718"
  "role": "android"
  "features": ["control.v1", "audio.pcm16-48k-mono", "session.v1"]
  "remote_actions": ["config.read", "config.write"]
  "limits": {
    "max_control_payload_bytes": 16384
    "max_audio_datagram_bytes": 1200
    "max_reorder_ms": 20
    "max_config_keys": 32
    "max_request_id_len": 64
  }
  "config_keys": [
    { "key": "audio.gain_db", "scope": "shared", "type": "number", "minimum": -24, "maximum": 24 }
    { "key": "ui.theme", "scope": "local", "type": "string" }
  ]
}
```

- `remote_actions` sólo admite `config.read` y `config.write`. **No existe** una acción remota para iniciar o parar el micrófono, conceder permisos ni cambiar claves locales.
- `mic.start`, `mic.stop` y `permission.request` son acciones **locales del dispositivo Android**; no tienen tipo de mensaje en v1. Un cliente que las pida por control recibe `unsupported` (o `local_key` si la petición es de configuración).
- `config_keys` declara alcance `shared`, `local` o `readonly`; una clave local no se comparte entre dispositivos.
- `audio.pcm16-48k-mono` es el perfil de audio base y debe anunciarse para recibir audio. `audio.opus-48k-mono` es opcional: habilita el perfil `type=2` sólo si **ambos** extremos la anuncian; si falta en cualquiera, se usa PCM16 (§8.3).
- `audio.return-pcm16-48k-mono` es opcional y declara el **retorno** Linux→Android (§8.4): sólo se envía si **ambos** extremos la anuncian y la clave local `audio.return_enabled` vale `true`.

## 8. Framing de audio

Un datagrama de audio contiene **exactamente un frame**. Cabecera de **28 bytes**, big-endian, seguida de payload. El perfil base (`type=1`) usa PCM16LE mono de 480 bytes; la extensión compatible `type=2` transporta un paquete Opus variable (§8.3):

| Offset | Bytes | Campo | Tipo | Valor / límite v1 |
|---:|---:|---|---|---|
| 0 | 1 | `type` | u8 | `1` (audio PCM16LE) o `2` (audio Opus; §8.3) |
| 1 | 1 | `version` | u8 | `1` |
| 2 | 1 | `flags` | u8 | bits 0–1; `0x01` silence, `0x02` discontinuity; bits 2–7 deben ser 0 |
| 3 | 1 | `header_len` | u8 | `28` |
| 4 | 4 | `epoch` | u32 BE | `1..=4294967295`; `0` inválido |
| 8 | 4 | `seq` | u32 BE | `0..=4294967295`; +1 por frame, wrap permitido |
| 12 | 2 | `sample_count` | u16 BE | `240` en type=1; `120`, `240` o `480` en type=2 |
| 14 | 2 | `duration_us` | u16 BE | `5000` en type=1; `2500`, `5000` o `10000` en type=2, coherente con `sample_count` |
| 16 | 4 | `sample_rate_hz` | u32 BE | `48000` |
| 20 | 2 | `channels` | u16 BE | `1` |
| 22 | 2 | `reserved` | u16 BE | `0`; otro valor ⇒ rechazo |
| 24 | 4 | `payload_len` | u32 BE | `480` en type=1; `1..=1172` en type=2 |
| 28 | variable | payload | bytes | type=1: PCM16LE, 240 muestras (480 bytes); type=2: un paquete Opus (1..=1172 bytes) |

Total del datagrama: **508 bytes** para type=1; **29..=1200 bytes** para type=2. Tamaño máximo aceptado: **1200 bytes** (presupuesto conservador de datagrama QUIC). Un datagrama mayor, con `payload_len` distinto de `sample_count × channels × 2` (type=1) o distinto de los bytes recibidos (type=2), o con constantes distintas se rechaza y se cuenta; no se fragmenta a este nivel.

### 8.1 Semántica de época y secuencia

- `epoch` identifica una generación del flujo. Empieza en `1` al autorizar la sesión y se incrementa al reiniciar el audio o cambiar el camino que altere el formato/reloj. `0` es inválido.
- `seq` empieza en `0` en cada época y se incrementa en 1 por frame; el wrap a `0` es válido y se detecta por época/tiempo, no por comparación ingenua.
- Un frame con `epoch` menor que el actual es **viejo**: se descarta, no se reproduce.
- Un `seq` duplicado en la misma época se descarta (no se reproduce dos veces).
- Un hueco de `seq` no se recupera por retransmisión: el receptor decide PLC/silencio y lo cuenta. Nunca se reordena audio más allá de la ventana de reordenamiento (**20 ms**).
- `flags.silence` indica payload silencioso; el payload sigue presente (480 bytes en type=1; 1..=1172 bytes en type=2). `flags.discontinuity` marca el primer frame tras un hueco o cambio de época; el receptor reinicia su ventana de jitter.
- El emisor no envía audio antes de que la sesión esté autorizada. El receptor descarta y cuenta cualquier datagrama de audio recibido en `enroll` o antes de la autorización.

### 8.2 Cadencia y colas

- Cadencia nominal: **200 frames/s** (5 ms) en el perfil de 240 muestras; con 120 o 480 muestras la cadencia escala para mantener 48 000 muestras/s (§8.3). Tolerancia de ráfaga del receptor: 10 frames; tasa sostenida máxima 250 frames/s en el perfil de 5 ms.
- El emisor mantiene una cola de salida muy corta (≤ 2 frames) y purga antes de enviar; el audio viejo se descarta, nunca se recupera.
- El receptor usa un jitter buffer acotado (objetivo configurable 5–40 ms) y descarta por deadline. El límite exacto de capacidad se fija en; el contrato no permite crecimiento por ráfaga.
- El desfase de reloj (drift) se corrige en el consumidor con contador de muestras y resampling lento; no se comparan relojes monotónicos Android/Linux directamente.

### 8.3 Perfil Opus negociado (type=2)

Extensión compatible de v1 para transportar Opus sin cambiar el perfil base. La cabecera es la misma de 28 bytes; sólo cambian los valores permitidos y el payload:

| Campo | Valor en type=2 |
|---|---|
| `type` | `2` |
| `version`, `header_len`, `sample_rate_hz`, `channels`, `reserved` | iguales que type=1 (`1`, `28`, `48000`, `1`, `0`) |
| `flags` | mismos bits 0–1; bits 2–7 en 0 |
| `sample_count` | `120`, `240` o `480` |
| `duration_us` | `2500` con 120, `5000` con 240, `10000` con 480 |
| `payload_len` | `1..=1172` (1200 − 28) |
| payload | exactamente **un paquete Opus** (RFC 6716), opaco para el framing |

- El datagrama completo mide `28 + payload_len`, es decir **29..=1200 bytes**; `payload_len` debe coincidir con los bytes recibidos. Un `payload_len` en `0` o mayor que `1172`, un `sample_count` fuera de `{120,240,480}`, una `duration_us` incoherente o cualquier constante alterada se rechaza y se cuenta, igual que en type=1.
- **Negociación:** `type=2` sólo se usa si **ambos** extremos anuncian la capacidad `audio.opus-48k-mono` en `capabilities` (§7). Si falta en cualquiera de los dos, el emisor usa `type=1` PCM16LE y el receptor no debe recibir type=2. Un receptor que recibe type=2 sin haberlo negociado descarta el datagrama y lo cuenta; no hay mensaje de control por datagrama.
- **Cambio de perfil:** al pasar de PCM16 a Opus o al revés se incrementa `epoch`, `seq` vuelve a `0` y el primer frame lleva `flags.discontinuity`. El receptor reinicia jitter y PLC con la nueva época.
- **Cadencia:** se mantienen 48 000 muestras/s: 400 fps con 120 muestras (2.5 ms), 200 fps con 240 (5 ms) y 100 fps con 480 (10 ms); la ráfaga permitida sigue siendo 10 frames. `flags.silence` no elimina el payload: un frame silencioso sigue llevando un paquete Opus válido (DTX, si se usa, es decisión del codificador).
- **Fuera de contrato:** bitrate, complexity, DTX/FEC, pre-skip del encoder y cualquier otro parámetro interno de Opus.

El esquema [`contracts/audio-frame-v1.schema.json`](../contracts/audio-frame-v1.schema.json) mantiene el descriptor raíz para type=1 y describe type=2 en `$defs.audio_frame_opus`; `x-audio-profiles` enumera ambos perfiles y su capacidad.

### 8.4 Retorno Linux→Android (`type=1`, /)

Dirección inversa, opt-in y apagada por defecto (, U18 del 2026-11-19):

- Reutiliza el frame `type=1` de §8 sin cambios: PCM16LE, 48 kHz, mono, 240 muestras y 480 bytes de payload. No hay perfil nuevo ni cabecera nueva.
- Sólo se envía si **ambos** extremos anuncian `audio.return-pcm16-48k-mono` en `capabilities` **y** la clave local del daemon `audio.return_enabled` vale `true`. Si falta cualquiera de las dos condiciones, el daemon **no envía ni un datagrama** de retorno.
- El retorno es un flujo independiente del micrófono Android→Linux: tiene su propia `epoch` y `seq`, y nunca se mezcla con el flujo de captura en el mismo datagrama ni en la misma época.
- El extremo Android debe mostrar un aviso de eco/feedback antes de activar el retorno; sin ese aviso no se activa. La captura de audio de apps Android queda fuera de alcance.
- Cómo se apaga y qué queda fuera de alcance: [`return-audio.md`](return-audio.md).

## 9. Límites exactos v1

| Elemento | Límite |
|---|---|
| Prefijo de longitud de control | 4 bytes BE |
| Payload de control | 1..=16384 bytes |
| Frame de control total | ≤ 16388 bytes |
| Profundidad JSON | 8 |
| Claves por objeto / elementos por array | 64 / 64 |
| String JSON | 1024 caracteres |
| Eventos | `config.changed`/`session.state`; `data` cerrada y plana (sin anidar objetos ni arrays) |
| `request_id` | 1..=64 caracteres `[A-Za-z0-9_-]` |
| Caché de idempotencia | 64 entradas, TTL 60 s, FIFO, por sesión |
| Cabecera de audio | 28 bytes |
| Payload de audio | 480 bytes (240 muestras × 1 canal × 2 bytes) |
| Muestras por frame Opus | 120, 240 o 480 (2.5, 5 o 10 ms a 48 kHz) |
| Payload de audio Opus | 1..=1172 bytes (exactamente un paquete Opus) |
| Datagrama de audio | 508 bytes; máximo aceptado 1200 |
| Datagrama de audio Opus | 29..=1200 bytes; máximo aceptado 1200 |
| Frames por datagrama | 1 |
| Ventana de reordenamiento | 20 ms |
| Cadencia / ráfaga | 200 fps nominal; 250 fps sostenido; ráfaga 10 |
| Jitter objetivo | 5–40 ms (configurable; valor por defecto 10 ms) |

Estos límites son parte del contrato: y deben aplicarlos y probarlos. Un límite distinto requiere revisión del contrato, no un valor «razonable» en el código.

## 10. Versionado y compatibilidad

- El major vive en `schema_version` (`arl.control/v1`, `arl.capabilities/v1`, `arl.audio-frame/v1`). Un major desconocido se rechaza con `version_unsupported`; no se intenta interpretar.
- v1 no acepta campos desconocidos. Cambios aditivos compatibles se declaran explícitamente en una revisión documentada del esquema; los incompatibles abren v2.
- El perfil de audio base es PCM16LE mono 48 kHz fijo. La extensión compatible `type=2` (Opus) se negocia con la capacidad `audio.opus-48k-mono` (§8.3); si falta en cualquiera de los dos extremos se usa PCM16. Cualquier otro códec o parámetro de wire requiere una fase posterior y un contrato nuevo.
- La sesión valida versión y límites **antes** de aceptar audio o control operativo.

## 11. Lo que este contrato no define

- Handshake TLS, pin SPKI, identidad de cliente, mTLS/Keystore o app-layer: [`security/pairing.md`](security/pairing.md) y el gate (/ADR-0002).
- Transporte concreto (quiche/Quinn), descubrimiento mDNS y puertos: decisión de.
- Parámetros internos de Opus (bitrate, complexity, DTX/FEC, pre-skip), DSP/reducción de ruido y métricas de latencia: fases. El wire v1 sólo transporta el paquete Opus opaco de §8.3.
- API de la CLI y del socket Unix: se fija en; aquí sólo se asume que el daemon es la autoridad de configuración.
