# Configuración v1 — requested/effective y claves locales


## 1. Modelo

- Toda operación lleva `request_id`, y `config.set` exige `expected_revision`.
- El daemon responde `ack` con tres cosas separadas:
  - **`requested`**: lo que el cliente pidió;
  - **`effective`**: lo que el daemon aplicó realmente (puede diferir por límites del dispositivo/SO);
  - **`revision`**: la revisión resultante.
- `applied: true` significa que el estado cambió y la revisión avanzó +1. `applied: false` significa que el valor efectivo ya era el actual; no hay cambio de revisión.
- Un conflicto de revisión no aplica nada y devuelve `error` con `conflict` y `current_revision`.
- No hay aplicación parcial: si una clave o un valor es inválido, la operación completa se rechaza.
- La revisión es **por proceso del daemon**; no se promete monotonicidad entre reinicios. Al reconectar, el cliente pide el snapshot (`config.get` con `key: null`).

## 2. Claves v1

| Clave | Tipo | Rango / valores | Default | Alcance | Notas |
|---|---|---|---|---|---|
| `audio.sample_rate_hz` | integer | `48000` (constante en v1) | `48000` | shared | El remoto puede leerla; en v1 no hay otra tasa. `effective` puede reflejar la tasa real del dispositivo. |
| `audio.frame_duration_ms` | integer | `5` (constante en v1) | `5` | shared | Un frame = 240 muestras a 48 kHz. |
| `audio.gain_db` | number | `-24.0..=24.0`, múltiplo de `0.5` | `0.0` | shared | Ganancia de captura; el valor efectivo puede quedar limitado por el dispositivo. |
| `audio.mute` | boolean | `true` / `false` | `false` | shared | Silencia el flujo; no detiene el micrófono ni el FGS. |
| `audio.jitter_target_ms` | integer | `5..=40` | `10` | shared | Objetivo del jitter buffer del receptor Linux. |
| `audio.return_enabled` | boolean | `true` / `false` | `false` | **local** | Habilita el retorno de audio Linux→Android (,); apagado por defecto. Ver [`return-audio.md`](return-audio.md). |
| `ui.theme` | string | `system`, `light`, `dark` | `system` | **local** | Preferencia visual del dispositivo; no se comparte. |
| `ui.text_scale` | number | `1.0..=2.0` | `1.0` | **local** | Accesibilidad local; no se comparte. |
| `device.local_volume` | number | `0.0..=1.0` | `1.0` | **local** | Volumen local del dispositivo; no es ganancia del flujo. |
| `device.model` | string | ≤64 caracteres | — | **readonly** | Informativa; el remoto no puede escribirla. |

Reglas:

- Las claves **local** y **readonly** nunca se aceptan por el canal remoto: `config.set` sobre ellas devuelve `local_key` (local) o `denied` (readonly) y **no** aplica nada. El panel Android las gestiona en su propio almacenamiento; la CLI local puede leerlas si el daemon las expone, pero no forman parte de la configuración compartida.
- `mic.start`, `mic.stop` y `permission.request` **no son claves de configuración** ni acciones remotas. Son acciones locales del dispositivo Android; no existe mensaje de control que las ejecute en v1.
- `audio.gain_db` no es `device.local_volume`: la primera afecta al flujo capturado; la segunda es reproducción local y no se comparte.
- `audio.return_enabled` es **local del daemon** y no se comparte: el panel Android no la escribe por el canal remoto. El retorno además exige que ambos extremos anuncien la feature `audio.return-pcm16-48k-mono`; sin las dos condiciones el daemon no envía audio de retorno (§ [return-audio.md](return-audio.md)).
- El valor efectivo de `audio.sample_rate_hz` y `audio.frame_duration_ms` se publica para diagnóstico; en v1 debe ser 48000 y 5. Si el dispositivo no puede, la sesión no se autoriza con ese formato (no se inventa un remuestreo silencioso en el contrato).

## 3. Ejemplo de intercambio

```json
{ "schema_version": "arl.control/v1", "type": "config.set", "request_id": "c-17"
  "expected_revision": 7
  "payload": { "key": "audio.gain_db", "value": 3.0 } }
```

```json
{ "schema_version": "arl.control/v1", "type": "ack", "request_id": "c-17", "revision": 8
  "payload": { "applied": true
               "requested": { "key": "audio.gain_db", "value": 3.0 }
               "effective": { "key": "audio.gain_db", "value": 3.0 } } }
```

Conflicto de revisión:

```json
{ "schema_version": "arl.control/v1", "type": "error", "request_id": "c-18"
  "payload": { "code": "conflict", "message": "revision mismatch", "retryable": true
               "current_revision": 8 } }
```

Clave local:

```json
{ "schema_version": "arl.control/v1", "type": "error", "request_id": "c-19"
  "payload": { "code": "local_key", "message": "key is local to the device", "retryable": false } }
```

## 4. Archivo local

Propuesta (la implementa): `$XDG_CONFIG_HOME/arl/config.toml`, sin secretos, escritura atómica y permisos del usuario.

```toml
# Sólo configuración compartida y preferencias locales del daemon Linux.
[audio]
sample_rate_hz = 48000
frame_duration_ms = 5
gain_db = 0.0
mute = false
jitter_target_ms = 10
return_enabled = false

[ui]
theme = "system"
text_scale = 1.0
```

- Claves desconocidas o fuera de rango ⇒ el daemon no arranca con esa configuración y reporta el error; no se ignora en silencio.
- El archivo no contiene token de enrolamiento, claves privadas, ACL ni material de identidad. Esos viven en el estado protegido del daemon (/), no en `config.toml`.
- Los valores por defecto se aplican sólo si la clave falta; un valor inválido nunca se sustituye por el default en silencio.

## 5. Validación y errores

| Situación | Resultado |
|---|---|
| Clave inexistente | `invalid` (no retryable) |
| Clave local/readonly por canal remoto | `local_key` / `denied` (no retryable) |
| Tipo incorrecto | `invalid` (no retryable) |
| Valor fuera de rango o no múltiplo de 0.5 en `gain_db` | `invalid` (no retryable) |
| `expected_revision` distinto | `conflict` (retryable tras releer) |
| `request_id` reutilizado con otro payload | `request_id_reuse` (no retryable) |
| Valor igual al efectivo actual | `ack` con `applied: false`, sin cambio de revisión |

El catálogo completo y el mapeo a códigos de salida CLI están en [`contracts/errors.md`](../contracts/errors.md).

## 6. Límites

- Máximo **32** claves declaradas en `capabilities.config_keys`.
- `request_id` de 1 a 64 caracteres; `key` de 1 a 64 caracteres con patrón `^[a-z][a-z0-9_]*(\.[a-z0-9_]+){0,3}$`.
- Strings de valor ≤1024 caracteres; números en el rango declarado por clave.
- La caché de idempotencia es de 64 entradas y 60 s por sesión (ver [`protocol.md`](protocol.md) §6).
