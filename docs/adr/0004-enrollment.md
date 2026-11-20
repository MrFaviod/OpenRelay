# ADR-0004 — Enrolamiento: pendiente, aprobación local e identidad persistente

- **Estado:** Accepted. Describes enrollment as implemented.
- **Ámbito:** `crates/transport/src/enrollment.rs` y `crates/daemon/src/pairing.rs`.

## Decisiones

1. **El cliente declara su identidad.** `enroll.request` lleva
   `"spki"`: hex del `SubjectPublicKeyInfo` DER de la clave que usará en mTLS
   (en Android, la del Keystore). El servidor lo valida con el parser
   `x509-cert` y calcula el pin SHA-256 con `SpkiPin::from_spki_der`.
2. **`Accepted` lleva el pin.** `EnrollmentEvent::Accepted(SpkiPin)`. El
   servidor no escribe nada: sólo entrega el pin al daemon.
3. **Pendiente en memoria.** `Pairing::observe` guarda el pin en una lista
   volátil. Un reinicio descarta los pendientes; el cliente vuelve a enrolar.
4. **Aprobación local explícita.** `Pairing::approve(pin, caps, path)` es la
   única vía de persistencia. Rechaza pines no pendientes. `reject` sólo
   descarta el pendiente.
5. **Sin ACL implícita.** Una identidad enrolada no obtiene capacidades. Hasta
   `approve`, `SessionServer` la rechaza con `NoAclEntry` (default deny).

## Formato QR

```
openrelay://pair?v=1&h=<host:port>&pin=<64 hex>&t=<64 hex>
```

- `pin`: SPKI del certificado del servidor de enrolamiento. El cliente
  aborta el handshake si no coincide, antes de enviar el token.
- `t`: token de un solo uso (32 bytes).

## Persistencia

- Archivo JSON: `{"version":1,"identities":[{"spki":"<64 hex>","mic":bool,
  "control_read":bool,"control_write":bool,"return_audio":bool}]}`.
- Escritura atómica: `<path>.tmp` creado con `create_new` y modo `0600`,
  `sync_all`, luego `rename`.
- `load` de un archivo inexistente devuelve ACL vacía. Versión o forma
  desconocida es error (fail-closed).

## Límites

- Revocación con `openrelay devices remove`: retira la identidad y cierra la
  sesión abierta.
- Sin UI en la app: la aprobación pasa por el daemon/CLI.
- El token sigue con TTL ≤ 300 s y un solo uso por ventana.
- El cliente no recibe ninguna capacidad por el enrolamiento.
