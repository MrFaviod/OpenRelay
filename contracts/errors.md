# Errores — contrato v1


## 1. Forma en el wire

```json
{
  "schema_version": "arl.control/v1"
  "type": "error"
  "request_id": "c-17"
  "payload": {
    "code": "conflict"
    "message": "revision mismatch"
    "retryable": true
    "current_revision": 9
  }
}
```

| Campo | Tipo | Obligatorio | Límite |
|---|---|---|---|
| `code` | string | sí | uno de la tabla §2 |
| `message` | string | sí | 1..=200 caracteres; sin secretos, tokens, claves ni rutas internas |
| `retryable` | boolean | sí | según la tabla; el cliente no lo deduce del texto |
| `current_revision` | entero | sólo `conflict` | `0..=2^53−1`; revisión actual del daemon |

`request_id` se copia del mensaje que causó el error. Si el error es de framing (no se pudo leer un `request_id`), se cierra la conexión y se registra el contador; no se inventa un `request_id`.

## 2. Códigos

| Código | Significado | Retryable | Cierra conexión | Origen típico |
|---|---|---:|---:|---|
| `invalid` | Mensaje, campo, tipo, valor o longitud inválidos | no | no | Validación de esquema o de clave |
| `version_unsupported` | `schema_version` o protocolo desconocido | no | sí | `hello` con major distinto |
| `request_id_reuse` | Mismo `request_id` con payload distinto | no | no | Reintento mal formado |
| `conflict` | `expected_revision` no coincide | sí | no | `config.set` concurrente |
| `local_key` | Clave de alcance local enviada por el canal remoto | no | no | `config.set` sobre `ui.*` o `device.local_volume` |
| `denied` | Operación no permitida por ACL o clave readonly | no | no | Capacidad no concedida |
| `unsupported` | Operación o tipo no soportado en v1 | no | no | Acción remota de micrófono, códec futuro |
| `busy` | Recurso ocupado; reintentar más tarde | sí | no | Ventana de enrolamiento activa, cambio en curso |
| `unauthorized` | Identidad ausente, inválida o revocada | no | sí | Sesión sin `peer_cert`/ACL o app-layer fallido |
| `too_large` | Frame o mensaje supera el límite declarado | no | sí | Control >16384 bytes o datagrama >1200 |
| `not_found` | Clave, sesión o identidad inexistente | no | no | `config.get` de clave desconocida |
| `internal` | Fallo interno sin detalle sensible | sí | no | Error inesperado del daemon |

Reglas:

- `retryable: true` no autoriza a reintentar en bucle: el cliente aplica backoff acotado y, en `conflict`, relee la revisión antes de reintentar.
- `unauthorized`, `version_unsupported` y `too_large` cierran la conexión: no se sigue negociando en un canal no confiable o desincronizado.
- Un error nunca incluye el token de enrolamiento, material de claves, audio, rutas del sistema ni trazas internas.
- Los errores de validación de un `config.set` no aplican ningún cambio; no hay aplicación parcial.

## 3. Errores de audio

El audio no tiene ACK ni mensajes de error por frame. Un datagrama inválido se **descarta y se cuenta**:

| Condición | Acción | Contador |
|---|---|---|
| `type`/`version`/`header_len` distintos | descartar | `audio.invalid_header` |
| `payload_len` ≠ `sample_count × channels × 2` | descartar | `audio.invalid_length` |
| `epoch == 0`, `reserved != 0` o flags con bits 2–7 | descartar | `audio.invalid_header` |
| Datagrama >1200 bytes | descartar | `audio.too_large` |
| Audio en `enroll` o antes de autorizar | descartar y registrar el intento | `audio.before_authorization` |
| `epoch` viejo, `seq` duplicado o fuera de ventana | descartar | `audio.stale` / `audio.duplicate` |
| Hueco de secuencia | PLC/silencio del receptor | `audio.lost` |

Una ráfaga sostenida de datagramas inválidos o audio pre-autorización es un evento de seguridad: el daemon puede cerrar la sesión y debe registrarlo sin incluir payload.

## 4. Mapeo a la CLI

| Código de error | Exit CLI propuesto |
|---|---:|
| `invalid` | 2 (uso) |
| `conflict` | 3 (conflicto/revisión) |
| `local_key`, `denied`, `unauthorized` | 4 (no autorizado) |
| `busy`, `not_found` | 5 (no disponible) |
| timeout de transporte local | 6 (timeout) |
| `unsupported`, `version_unsupported` | 7 (no soportado) |
| `too_large`, `request_id_reuse`, `internal` | 1 (error general) |

El exit code es una comodidad para scripts; la fuente de verdad es el `code` del envelope. Un `error` con `retryable: true` no se traduce a éxito.

## 5. Precedencia

Cuando varios problemas coinciden, el daemon reporta en este orden:

1. framing/versión (cierra conexión);
2. autenticación/autorización (`unauthorized`, `denied`);
3. tamaño (`too_large`);
4. validación de forma (`invalid`);
5. conflicto de revisión (`conflict`);
6. alcance de clave (`local_key`);
7. estado (`busy`, `not_found`);
8. interno (`internal`).

Este orden es normativo para que los tests de no dependan del texto del mensaje.
