# Emparejamiento y sesión — requisitos de diseño


## 1. Dos superficies, no una

| Superficie | Certificado de servidor | Certificado de cliente | Acepta | Nunca acepta |
|---|---|---|---|---|
| **Enrolamiento** (`enroll`) | Efímero de la ventana, pin SPKI en el QR | No | Mensajes de enrolamiento, token y prueba de pin | Audio, control operativo, tickets, ACL persistente |
| **Sesión** (`session`) | Identidad persistente del daemon | Sí: mTLS con `peer_cert` + ACL, o app-layer aprobado con binding TLS | Audio, control, tickets y sesión | Cliente sin identidad válida o sin autorización |

- Son **endpoints distintos**, con certificado de servidor distinto y conjuntos de mensajes disjuntos. El cliente reconecta a `session` después de la aprobación; no hay transición in-band de `enroll` a `session`.
- El pin del QR pertenece **sólo** al certificado efímero de enrolamiento. **No se reutiliza** como ancla de las sesiones siguientes.
- El enrolamiento puede aceptar un cliente sin certificado; la ausencia de certificado no es una sesión y no habilita audio ni control.

## 2. Flujo de enrolamiento

1. **Apertura local.** El usuario ejecuta `arl pair open --ttl 120` en una TTY. El daemon crea una ventana de enrolamiento aislada con:
   - certificado de servidor efímero `S_e` y su pin SPKI;
   - token aleatorio de un solo uso (256 bits, codificado en el QR);
   - caducidad estricta (propuesta 120 s, máximo configurable acotado);
   - endpoint de enrolamiento anunciado en el QR.
   El token **no** se escribe en disco, no se emite con `--json` y no se registra en logs.
2. **Descubrimiento.** El QR es el canal principal. mDNS, si se usa, es **sólo descubrimiento**: un nombre anunciado no aporta confianza. También es válido introducir el endpoint a mano.
3. **Conexión y validación de pin.** Android conecta a `enroll` y valida el pin SPKI de `S_e` **antes de enviar el token**. Si el pin no coincide, aborta sin revelar el token.
4. **Token.** Sólo tras validar el pin, Android envía el token de un solo uso. El servidor lo compara en tiempo constante, comprueba vigencia y ventana, y lo invalida al usarlo.
5. **Aprobación local.** La terminal muestra identidad/fingerprint del cliente y un código corto. El usuario aprueba o rechaza. Sin aprobación no se provisiona nada.
6. **Provisión.** Sólo tras aprobar se crea la identidad persistente del cliente y se escribe la ACL (default deny, capacidades mínimas). El cliente reconecta al endpoint `session`.
7. **Cierre de ventana.** Aprobar, cancelar o expirar destruye la ventana, el certificado efímero, el token y cualquier estado temporal.

## 3. Flujo de sesión

1. Android conecta al endpoint `session` con su identidad persistente.
2. El servidor valida la cadena/ancla de confianza y exige identidad de cliente:
   - **camino mTLS:** certificado de cliente válido, `peer_cert` presente y ACL por identidad;
   - **camino app-layer:** challenge-response estándar revisado y binding al canal TLS, según ADR-0002.
   `verify_peer(true)` por sí solo **no** acredita un cliente: la comprobación de `peer_cert == None` es obligatoria en el camino mTLS.
3. El servidor responde con versión de protocolo, límites y snapshot de configuración (revisión incluida).
4. Sólo entonces se abre la sesión de audio. El primer frame lleva `epoch` nuevo y `flags` de discontinuidad; el receptor descarta cualquier audio anterior a la autorización.
5. La sesión termina por parada local Android, `arl session stop`, revocación o pérdida de conexión. Al terminar, el nodo de audio deja de producir y se invalidan los tickets de esa sesión.

## 4. Rechazos obligatorios

| Caso | Resultado |
|---|---|
| Token reutilizado, vencido o de otra ventana | Rechazo; no se provisiona identidad; se registra el intento sin el token. |
| Pin SPKI distinto o certificado caducado | El cliente aborta antes de enviar el token. |
| Cliente sin identidad válida en `session` (mTLS) | Rechazo antes de control/audio. |
| Audio, control o ticket en `enroll` | Rechazo explícito y cierre; se cuenta el intento. |
| Aprobación local denegada | Se destruye la ventana; no queda identidad ni ACL. |
| Estado local perdido | El cliente debe re-enrolar; **nunca** se acepta un servidor o cliente nuevo en silencio (sin TOFU). |
| Repetición de un mensaje de enrolamiento fuera de orden | Rechazo; la ventana es de un solo uso y un solo cliente. |
| Flood o mensajes sobredimensionados | Rechazo por límites antes del parseo profundo; cuotas por IP/identidad. |

## 5. Identidad, ACL y revocación

- **Identidad por dispositivo**, no por conexión. La ACL se indexa por identidad y por capacidad: `mic`, `control.read`, `control.write`, `return` (futuro). Default deny.
- **Tickets** de vida corta ligados a identidad y sesión; no sustituyen la autenticación de la conexión.
- **Revocación** elimina la identidad, cierra sus sesiones activas e invalida sus tickets. No borra evidencia ni el historial de la decisión.
- **Rotación** de identidad del servidor: si cambia, el cliente debe volver a validar el nuevo ancla; no se acepta un cambio silencioso.
- **Límite de confianza:** la aprobación local es humana. El código corto y el fingerprint reducen el error; no lo eliminan.

## 6. Requisitos del QR (propuestos, formato final en)

El QR debe contener, como mínimo:

- versión del formato de enrolamiento;
- endpoint de enrolamiento (host/puerto o equivalente);
- pin SPKI del certificado efímero `S_e`;
- token de un solo uso;
- caducidad absoluta.

No debe contener: identidad persistente, clave privada, token de sesión, configuración ni datos personales. El QR se muestra sólo en la terminal local; no se persiste ni se envía a terceros.

## 7. Límites y supuestos

- Ventana de enrolamiento: TTL propuesto 120 s; una sola ventana activa por daemon; un solo cliente por ventana.
- El enrolamiento no transporta audio ni control: no hay nada que proteger con una sesión antes de aprobar.
- La LAN es hostil; el flujo no depende de que la red sea confiable.
- El puente Keystore/mTLS en Android es un **pendiente bloqueante de ** (/ADR-0002). Si no hay bridge de firma, se elige app-layer estándar revisado o se declara la limitación; no se copia un PEM desde Keystore ni se inventa criptografía.
- Los nombres exactos de mensajes, su codificación y el handshake se fijan en / con tests negativos (`tls_identity_gate`, `enrollment_boundary`, `pairing_flow`).

## 8. Relación con otros documentos

- Activos, adversarios y riesgos residuales: [`threat-model.md`](threat-model.md).
- Contrato de control/audio y límites: [`../protocol.md`](../protocol.md).
- Configuración compartida y claves locales: [`../config-schema.md`](../config-schema.md).
