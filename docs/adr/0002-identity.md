# ADR-0002 — Identidad de cliente: mTLS con firma externa, y su límite Android

- **Estado:** Accepted in part; pairing as shipped is described in docs/security/pairing.md.
- **Fecha:** 2026-11-14.
- **Relacionado:** [ADR-0001](0001-transport.md), [emparejamiento](../security/pairing.md), [modelo de amenaza](../security/threat-model.md), [protocolo v1](../protocol.md).

## 1. Contexto

Los invariantes de seguridad exigen que el endpoint de sesión no entregue audio
sin identidad de cliente válida, y que el de enrolamiento no transporte audio
control ni tickets ([`docs/security/pairing.md`](../security/pairing.md)
[`docs/security/threat-model.md`](../security/threat-model.md)). El contrato v1
deja explícitamente fuera de su alcance el handshake, el pin SPKI y la identidad
de cliente: son de este ADR.

La restricción dura es Android: una clave de `AndroidKeyStore` **no es
exportable**. Cualquier diseño que necesite los bytes de la clave privada en
memoria del proceso queda descartado de entrada. `verify_peer(true)` por sí solo
no acredita un cliente, y en quiche esa opción **aún acepta clientes sin
certificado** si no se comprueba `peer_cert` explícitamente.

## 2. Decisión

**Camino mTLS con firma externa, sobre `rustls` 0.23.22.**

1. El servidor de sesión exige certificado de cliente mediante
   `WebPkiClientVerifier` construido sobre la **CA local**. No hay modo sin
   cliente en la superficie de sesión.
2. Tras el handshake, el servidor lee `peer_identity`, extrae el certificado
   hoja y calcula su **pin SPKI SHA-256**; sin identidad de peer rechaza con
   `RejectReason::NoPeerCertificate`. Una cadena válida sin entrada en la `Acl`
   se rechaza con `NoAclEntry`. La `Acl` es *default deny* por construcción:
   `Acl::new` está vacía y `Capabilities::none` no concede nada.
3. El cliente exige **cadena contra la CA local y pin SPKI del servidor**
   mediante `PinnedServerVerifier`, que ejecuta primero la verificación webpki
   normal (cadena, nombre, validez) y sólo después compara el pin. Un pin
   distinto produce `ApplicationVerificationFailure`. No se usa `verify=false`
   ni `danger_accept_invalid_certs`: el verificador es propio y **más** estricto
   que el estándar, no menos.
4. La firma del `CertificateVerify` se delega en un `rustls::sign::SigningKey`
   externo. Es el gancho real para Android: el trait recibe el mensaje a firmar
   y devuelve la firma, sin que la clave salga del proveedor. En host se
   ejercita con un firmador de test que cuenta firmas; **eso no es Keystore**.

### 2.1 Separación de superficies

| | Enrolamiento (`openrelay-enroll/1`) | Sesión (`openrelay-session/1`) |
|---|---|---|
| Certificado de cliente | **No se pide** (`with_no_client_auth`) | **Obligatorio** (`WebPkiClientVerifier`) |
| Identidad de peer | Ausente por diseño | `peer_cert` + pin SPKI + ACL |
| DATAGRAM | Deshabilitado (`datagram_receive_buffer_size(None)`, send 0) | Audio permitido tras autorizar |
| Tickets TLS | `send_tls13_tickets = 0` | `send_tls13_tickets = 0` |
| 0-RTT | `max_early_data_size = 0` / `enable_early_data = false` | Igual |
| Audio/control/ticket | Rechazado con cierre `0x210` | Permitido según capacidades |

El cliente de enrolamiento valida el **pin del certificado del servidor antes de
enviar el token**: `EnrollClient::connect` construye el verificador con pin y el
token sólo se escribe en `send_token`, que exige `is_ready`. Un pin incorrecto
aborta el handshake y el token nunca sale del dispositivo.

### 2.2 Ventana de enrolamiento

- Token de 32 bytes del RNG del sistema (`ring::rand::SystemRandom`), TTL
  `1..=300 s`, una sola ventana activa (`EnrollError::WindowOpen`) y **un solo
  uso**: al aceptar, la ventana se destruye (`self.window = None`), de modo que
  un segundo intento se rechaza con `NoWindow`. La variante `Reused` existe en
  el enum y en el `match`, pero hoy es inalcanzable por esa ruta.
- Comparación en tiempo constante; motivos de rechazo acotados:
  `NoWindow`, `Expired`, `Reused`, `Mismatch`, `Version`, `Malformed`.
- El enrolamiento sólo entrega el pin aprobado: `EnrollmentEvent::Accepted` no
  provisiona nada por sí mismo.
- `EnrollmentToken` no imprime su valor en `Debug`.

### 2.3 Parser SPKI

El pin se calcula extrayendo el `SubjectPublicKeyInfo` DER completo del
certificado hoja. La validación la hace el parser estándar `x509-cert` 0.2.5
(RFC 5280, DER estricto: longitudes canónicas, sin bytes sobrantes, tipos
correctos) y después un `der::SliceReader` recorre `tbsCertificate` saltando
versión opcional, serial, algoritmo de firma, emisor, validez y sujeto para
localizar el TLV del SPKI. Límites: cadena ≤ 4 certificados, certificado
≤ 16 KiB.

Esto **no** corrige un supuesto off-by-one previo: el diagnóstico que lo
apuntaba no quedó confirmado. El cambio se justifica por el requisito de usar un
parser estándar en vez de offsets propios.

## 3. Lo que Android todavía no acredita

Este ADR fija el camino, no el puente:

- **No hay puente JNI.** No se escribió ninguna clase Kotlin/Java que implemente
  `Signature.getInstance("SHA256withECDSA")` sobre una `PrivateKeyEntry` de
  Keystore, ni el `SigningKey`/`Signer` de Rust que la invoque.
- **No hay handshake Android real.** El `CertificateVerify` firmado por Keystore
  no se produjo nunca en un dispositivo.
- **No hay clave no exportable probada.** El firmador de host tiene la clave en
  memoria; demuestra el contrato del trait, no la propiedad de no exportación.

## 4. Alternativas rechazadas

| Alternativa | Motivo |
|---|---|
| **Exportar la clave de Keystore a PEM** | Keystore no exporta claves privadas; copiar un PEM es exactamente lo que el diseño prohíbe. No aprobado. |
| **Secreto en almacenamiento interno cifrado como fallback** | Cambia la promesa: la clave ya no es hardware no exportable. Exigiría aprobación explícita y un ADR propio; no es default. |
| **Identidad app-layer con challenge-response** | Prevista como camino alternativo si no hay puente de firma; exige un protocolo de autenticación estándar completo, revisado y ligado al canal TLS; disponer de `export_keying_material` no define por sí solo ese protocolo. No se elige mientras el camino mTLS sea viable. |
| **`verify_peer(true)` como sustituto de mTLS** | No acredita cliente: sigue aceptando conexiones sin certificado. Prohibido por el modelo de amenaza. |
| **TOFU silencioso** | Prohibido: una identidad desconocida no se acepta ni se recuerda sin aprobación local. |
| **0-RTT con reanudación** | Prohibido; además deshabilitado en ambos extremos. |
| **Certificado autofirmado por el cliente sin CA** | Sin ancla de confianza no hay autoridad que emitir ni revocar. |

## 5. Riesgos y límites declarados

- **El pin SPKI identifica la clave, no el dispositivo.** Un mismo certificado
  copiado a otro dispositivo da el mismo pin. La defensa es que la clave no sea
  exportable; mientras eso no se acredite en Android, la propiedad no está
  probada.
- **La ACL es un mapa en memoria** indexado por pin. La persistencia, la
  revocación y las cuotas por identidad se apoyan en el archivo de identidades
  del daemon y en `openrelay devices remove`.
- **`Capabilities` se evalúa en el servidor por operación** (`mic`
  `control_read`, `control_write`, `return_audio`), no en el cliente. Un cliente
  malicioso que abra un stream antes de autorizar recibe cierre `0x103`.
- **Sin cuotas medidas por identidad.** El modelo de amenaza pide «límites
  numéricos de flood y cuotas por identidad» con valores medidos; aquí sólo hay
  los topes estructurales de ADR-0001.
- **La ventana de enrolamiento admite un solo cliente** y no tiene límite de
  intentos por IP; `attempts` cuenta intentos pero no los limita.
- **`EnrollReject::Reused` no es alcanzable hoy.** Tras aceptar, la ventana pasa
  a `None`, así que el reintento se reporta como `NoWindow`. El rechazo es
  correcto (el segundo intento no entra), pero el motivo declarado en el enum no
  se ejerce.

## 6. Lo que este ADR no decide

- Persistencia de identidad, revocación y tickets.
- Formato exacto del QR y de los mensajes de emparejamiento.
- Almacenamiento de la CA local en Android y su provisión.
- El puente de firma Android queda como trabajo futuro; si no se puede cumplir
  la salida **no** es exportar la clave: es elegir el camino app-layer estándar
  revisado con binding TLS, o declarar la limitación explícitamente en una
  revisión de este ADR.
