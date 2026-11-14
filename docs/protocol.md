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

## 8. Framing de audio

Un datagrama de audio contiene **exactamente un frame**. Cabecera de **28 bytes**, big-endian, seguida de payload PCM16LE mono:

| Offset | Bytes | Campo | Tipo | Valor / límite v1 |
|---:|---:|---|---|---|
| 0 | 1 | `type` | u8 | `1` (audio) |
| 1 | 1 | `version` | u8 | `1` |
| 2 | 1 | `flags` | u8 | bits 0–1; `0x01` silence, `0x02` discontinuity; bits 2–7 deben ser 0 |
| 3 | 1 | `header_len` | u8 | `28` |
| 4 | 4 | `epoch` | u32 BE | `1..=4294967295`; `0` inválido |
| 8 | 4 | `seq` | u32 BE | `0..=4294967295`; +1 por frame, wrap permitido |
| 12 | 2 | `sample_count` | u16 BE | `240` (5 ms a 48 kHz) |
| 14 | 2 | `duration_us` | u16 BE | `5000` |
| 16 | 4 | `sample_rate_hz` | u32 BE | `48000` |
| 20 | 2 | `channels` | u16 BE | `1` |
| 22 | 2 | `reserved` | u16 BE | `0`; otro valor ⇒ rechazo |
| 24 | 4 | `payload_len` | u32 BE | `480` |
| 28 | 480 | payload | bytes | PCM16LE, 240 muestras |

Total del datagrama: **508 bytes**. Tamaño máximo aceptado: **1200 bytes** (presupuesto conservador de datagrama QUIC). Un datagrama mayor, con `payload_len` distinto de `sample_count × channels × 2` o con constantes distintas se rechaza y se cuenta; no se fragmenta a este nivel.

### 8.1 Semántica de época y secuencia

- `epoch` identifica una generación del flujo. Empieza en `1` al autorizar la sesión y se incrementa al reiniciar el audio o cambiar el camino que altere el formato/reloj. `0` es inválido.
- `seq` empieza en `0` en cada época y se incrementa en 1 por frame; el wrap a `0` es válido y se detecta por época/tiempo, no por comparación ingenua.
- Un frame con `epoch` menor que el actual es **viejo**: se descarta, no se reproduce.
- Un `seq` duplicado en la misma época se descarta (no se reproduce dos veces).
- Un hueco de `seq` no se recupera por retransmisión: el receptor decide PLC/silencio y lo cuenta. Nunca se reordena audio más allá de la ventana de reordenamiento (**20 ms**).
- `flags.silence` indica payload silencioso; el payload sigue presente y con el tamaño fijo. `flags.discontinuity` marca el primer frame tras un hueco o cambio de época; el receptor reinicia su ventana de jitter.
- El emisor no envía audio antes de que la sesión esté autorizada. El receptor descarta y cuenta cualquier datagrama de audio recibido en `enroll` o antes de la autorización.

### 8.2 Cadencia y colas

- Cadencia nominal: **200 frames/s** (5 ms). Tolerancia de ráfaga del receptor: 10 frames; tasa sostenida máxima 250 frames/s.
- El emisor mantiene una cola de salida muy corta (≤ 2 frames) y purga antes de enviar; el audio viejo se descarta, nunca se recupera.
- El receptor usa un jitter buffer acotado (objetivo configurable 5–40 ms) y descarta por deadline. El límite exacto de capacidad se fija en; el contrato no permite crecimiento por ráfaga.
- El desfase de reloj (drift) se corrige en el consumidor con contador de muestras y resampling lento; no se comparan relojes monotónicos Android/Linux directamente.

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
| Datagrama de audio | 508 bytes; máximo aceptado 1200 |
| Frames por datagrama | 1 |
| Ventana de reordenamiento | 20 ms |
| Cadencia / ráfaga | 200 fps nominal; 250 fps sostenido; ráfaga 10 |
| Jitter objetivo | 5–40 ms (configurable; valor por defecto 10 ms) |

Estos límites son parte del contrato: y deben aplicarlos y probarlos. Un límite distinto requiere revisión del contrato, no un valor «razonable» en el código.

## 10. Versionado y compatibilidad

- El major vive en `schema_version` (`arl.control/v1`, `arl.capabilities/v1`, `arl.audio-frame/v1`). Un major desconocido se rechaza con `version_unsupported`; no se intenta interpretar.
- v1 no acepta campos desconocidos. Cambios aditivos compatibles se declaran explícitamente en una revisión documentada del esquema; los incompatibles abren v2.
- El audio no negocia códec en v1: es PCM16LE mono 48 kHz fijo. Cualquier otro formato requiere una fase posterior y un contrato nuevo.
- La sesión valida versión y límites **antes** de aceptar audio o control operativo.

## 11. Lo que este contrato no define

- Handshake TLS, pin SPKI, identidad de cliente, mTLS/Keystore o app-layer: [`security/pairing.md`](security/pairing.md) y el gate (/ADR-0002).
- Transporte concreto (quiche/Quinn), descubrimiento mDNS y puertos: decisión de.
- Códec Opus, DSP/reducción de ruido y métricas de latencia: fases.
- API de la CLI y del socket Unix: se fija en; aquí sólo se asume que el daemon es la autoridad de configuración.
