# ADR-0001 — Transporte QUIC para audio (DATAGRAM) y control (stream fiable)

- **Estado: propuesto y condicionado. No congela.**
- **Ámbito:** `crates/transport` en la rama `work/f2-f3`, corte.
- **Fecha:** 2026-11-14.
- **Decide:** («¿qué transporte se congela para audio/control?»), sólo en su
  parte de motor; el congelamiento sigue sujeto a –.
- **Relacionado:** [ADR-0002](0002-identity.md), [protocolo v1](../protocol.md)
  [emparejamiento](../security/pairing.md), [acta /](../plan/execution-f2-f3.md).

## 1. Contexto

El contrato v1 fija las superficies y deja el motor abierto: el audio viaja como
un frame PCM16 por DATAGRAM y el control por un stream fiable con framing
`u32` big-endian + JSON UTF-8, con los límites de
[`docs/protocol.md`](../protocol.md) (§9) sin redefinir. La elección del motor
era una decisión de, con dos candidatos en el
[ledger](../dependency-ledger.md): quiche (C-03) y quinn (C-04).

Tres restricciones mandan sobre la elección:

1. La toolchain está fijada a **Rust 1.80.1** en `rust-toolchain.toml`.
2. La identidad debe poder firmar con una clave **no exportable** de Android
   , lo que exige un gancho de firma externa en el stack TLS.
3. El crate debe ser reutilizable en Linux y Android sin arrastrar un runtime
   asíncrono ni HTTP/3, porque el bucle de audio lo gobierna el llamador.

## 2. Decisión

Se adopta **`quinn-proto` 0.11.12** con **`rustls` 0.23.22** y **`ring` 0.17.8**
en modo **sans-IO**: `arl-transport` recibe y devuelve datagramas UDP y no abre
sockets ni hilos. El crate se declara en
[`crates/transport/Cargo.toml`](..../crates/transport/Cargo.toml):

| Dependencia | Pin | Papel |
|---|---|---|
| `quinn-proto` | `=0.11.12`, `default-features = false`, feature `rustls-ring` | Motor QUIC (endpoint, conexión, DATAGRAM, streams) |
| `rustls` | `=0.23.22`, features `std`, `ring` | TLS 1.3, verificadores y `SigningKey` externo |
| `ring` | `=0.17.8` | Primitivas criptográficas |
| `rustls-pki-types` | `=1.16.0` | Tipos de certificado y clave |
| `x509-cert` | `=0.2.5` | Parser DER estándar para localizar el SPKI |
| `bytes` | `=1.8.0` | Buffers sin copia del motor |

Se usa el **crate `quinn-proto`**, no el crate `quinn` 0.11.12: el segundo no
existe en esa versión y el primero es el que expone la API sans-IO con
`peer_identity`, `handshake_data` y `export_keying_material`.

### 2.1 Reparto de superficies

- **Audio:** DATAGRAM QUIC, un `AudioFrame` v1 por datagrama, sin fragmentación
  en este nivel. El datagrama de contrato son 508 B (cabecera 28 B + 480 B de
  payload) y el máximo aceptado es 1200 B
  ([`crates/audio-core/src/frame.rs`](..../crates/audio-core/src/frame.rs)).
  Un datagrama mayor cierra la conexión con código `0x101` en lugar de
  truncarse.
- **Control:** stream bidireccional fiable con `u32` BE de longitud + JSON, con
  `MAX_CONTROL_FRAME = 16388` (16384 de payload + 4 de longitud) tomado de
  `arl-proto`, no redeclarado aquí.
- **ALPN:** `arl-enroll/1` y `arl-session/1` son constantes separadas en
  [`crates/transport/src/lib.rs`](..../crates/transport/src/lib.rs). El
  servidor de sesión rechaza con `RejectReason::Alpn` cualquier handshake que no
  negocie `arl-session/1`, y esa comprobación ocurre **antes** de mirar la
  identidad.

### 2.2 Límites explícitos

Los valores viven en código y no son configurables fuera de rango:

| Límite | Valor | Dónde |
|---|---|---|
| Sesiones simultáneas | 8 | `MAX_SESSIONS` |
| Handshakes pendientes | 8 | `MAX_PENDING_INCOMING` |
| Audio en cola por sesión | 8 (descarta el más viejo) | `MAX_QUEUED_AUDIO` |
| Control en cola por sesión | 8 (descarta el más viejo) | `MAX_QUEUED_CONTROL` |
| Timeout de inactividad | 10 s por defecto, válido en `1..=60 s` | `SessionLimits::validate` |
| Streams de control concurrentes | 2 por defecto, válido en `1..=8` | `SessionLimits::validate` |
| Frame de control | ≤ `MAX_CONTROL_FRAME` | `SessionLimits::validate` |
| Datagrama de audio | ≤ `MAX_AUDIO_DATAGRAM` (1200 B) | `SessionLimits::validate` |

Una configuración fuera de rango devuelve `TransportError::Config`; no se
recorta en silencio.

### 2.3 Deshabilitados por construcción

- **0-RTT:** `tls.max_early_data_size = 0` en el servidor y
  `tls.enable_early_data = false` en el cliente. Además el cliente expone
  `has_0rtt` y `accepted_0rtt` para que la prueba afirme que nunca ocurre.
- **Tickets de sesión:** `send_tls13_tickets = 0`.
- **HTTP/3 y Tokio:** no hay dependencia ni tarea de fondo; el `Endpoint` se
  bombea con `poll_transmit`/`poll_timeout`/`handle_timeout` desde el llamador.

## 3. Consecuencias

- El socket de producción se cablea en / **sin cambiar esta frontera**: el
  crate no conoce `std::net::UdpSocket` ni JNI.
- El bucle de audio decide cuándo bombear, lo que permite un test determinista
  con enlace en memoria y, más adelante, un integrador en el hilo de red de
  Android.
- Al ser sans-IO, ninguna prueba de este crate acredita red real, latencia ni
  comportamiento de socket. Eso es y sigue abierto.
- La API pública (`SessionServer`, `SessionClient`, `SessionLimits`
  `Capabilities`, `Acl`, `RejectReason`, `ServerEvent`, `ClientEvent`
  `TransportError`) queda **entregada pero no liberada a consumidores**: y
   no deben empezar contra ella hasta que se cierren las regresiones R7/R8 de
  §4.

## 4. Requisitos observables

Cada fila es comprobable con un comando o una lectura concreta. La columna
«evidencia» distingue lo ejecutado por el padre de lo que sigue pendiente; un
check no ejecutado no es PASS.

| # | Requisito | Cómo se comprueba | Evidencia |
|---|---|---|---|
| R1 | Handshake QUIC real entre dos peers, no structs | `cargo test --locked -p arl-transport --test tls_identity_gate` | 7 pruebas, exit 0 (ejecutado por el padre) |
| R2 | Audio por DATAGRAM tras autorizar y roundtrip de `encode` | `authorized_session_delivers_real_audio_and_control` | Verde |
| R3 | Control por stream fiable en ambos sentidos | misma prueba, `Ping` y `Event`/`session.state` | Verde |
| R4 | ALPN separados: sesión no entra en enroll y viceversa | `enrollment_boundary`, pruebas `session_client_cannot_use_enrollment_endpoint` y `enrollment_client_cannot_use_session_endpoint` | Verde (8 pruebas en la suite) |
| R5 | 0-RTT desactivado; handshake normal no lo ofrece ni acepta | `assert!(!client.has_0rtt)`, `assert!(!client.accepted_0rtt)` | Verde sólo para handshake normal; falta intento adversarial |
| R6 | Límites rechazados fuera de rango | lectura de `SessionLimits::validate` | Presente en código; sin prueba dedicada de cada rama |
| R7 | Stream de control tolera `Blocked` y rechaza `Reset`/malformed | prueba de regresión dedicada | **Pendiente — pendiente** |
| R8 | Stream recién aceptado entrega bytes ya disponibles sin esperar otro evento Readable | prueba de regresión dedicada | **Pendiente — pendiente** |
| R9 | Empaquetado Android: ABI, páginas de 16 KB, tamaño, licencias | build real de APK.so | **Pendiente — ** |
| R10 | RTT/jitter/pérdida/CPU en LAN con `tc-netem` | medición en red real | **Pendiente — ** |

R7 y R8 son la razón por la que este ADR **no** declara la API liberada: el
acta registra dos rondas de corrección agotadas sin tercera automática
([acta /](../plan/execution-f2-f3.md)).

## 5. Alternativas rechazadas

| Alternativa | Motivo del rechazo |
|---|---|
| **quiche 0.30.1** (recomendación inicial de) | Declara `rust-version = 1.88`, por encima de la toolchain fijada 1.80.1; y su única carga de clave privada es `Config::load_priv_key_from_pem_file`, sin trait de firma externa. Incompatible con Keystore no exportable. |
| **quiche 0.22.0** | Compila en 1.80, pero mantiene la misma limitación: sólo clave desde fichero PEM. Descartado por identidad, no por motor. |
| **quinn-proto 0.11.19** | Declara MSRV 1.85, fuera de la toolchain. |
| **crate `quinn` con Tokio** | Añade un runtime asíncrono que el contrato de audio no necesita y complica el callback de tiempo real. |
| **HTTP/3** | No hay HTTP en el protocolo; añade superficie y dependencias sin requisito. |
| **SRTP + DTLS** | Sólo comparador si el gate falla; no cubre el stream de control con el framing de contrato. |
| **UDP claro con framing propio** | Prohibido por los invariantes de seguridad (sin fallback a UDP claro). |

## 6. Riesgos conocidos

- **Framing corrupto sin cierre (pendiente):** la revisión host y la lectura del padre confirman que `read_control` limpia el buffer y retorna sin cerrar ante longitud ilegal; los llamadores no reciben error. Incumple el cierre exigido por `docs/protocol.md` §3. No hay repro E2E; no se atribuye bypass de identidad. Los errores de contenido requieren `invalid` según contrato, no cierre indiscriminado. El cliente también descarta audio malformado sin cerrar; política y regresión pendientes.
- **Lectura del stream de control:** el camino de `ReadError::Blocked` conserva
  el prefijo y reintenta, pero no tiene regresión dedicada (R7). Un fallo aquí
  se manifestaría como control que deja de entregarse, no como error visible.
- **Motivo de rechazo impreciso en audio malformado:** en
  [`quinn.rs`](..../crates/transport/src/quinn.rs), un `AudioFrame::decode`
  fallido emite `ServerEvent::Rejected` con `RejectReason::Tls`, un motivo que
  corresponde a otra capa. No cambia el cierre fail-closed (código `0x102`)
  pero confunde la telemetría; corregirlo exige ampliar `RejectReason`, lo que
  es un cambio de API y no se hizo en este corte.
- **Sin medición:** ninguna cifra de latencia, CPU o memoria de este ADR es una
  medición; sigue abierta.

## 7. Lo que este ADR no decide

- Identidad, mTLS, Keystore y confianza del cliente: [ADR-0002](0002-identity.md).
- Descubrimiento mDNS y puertos: siguen siendo decisión de, no fijada aquí.
- Códec (Opus), DSP y métricas de latencia: fases.
- Formato exacto de QR y mensajes de emparejamiento:.
- **No se declara cerrada.** El motor está implementado y probado en host;
  la elección se congela cuando – estén verdes y el gate se firme.

## 8. Condiciones para pasar a «aceptado»

1.: modelo de confianza y pin SPKI definidos y probados, sin `verify=false`;
   el cliente rechaza un pin distinto (`identity_pin`, `identity.rs`).
2.: identidad de sesión probada — sin certificado, sin ACL y con CA ajena
   rechazados, `peer_cert` explícito (cubierto en host, ver ADR-0002).
3.: firma Android real (bloqueante de ADR-0002; sin ella el transporte no se
   congela aunque el motor esté verde).
4.: build Android real con la ABI declarada, páginas de 16 KB, tamaño y
   licencias de producto medidos, no inferidos.
5.: RTT, jitter, pérdida y CPU medidos en LAN con `tc-netem`.
6. R7 y R8 verdes con pruebas de regresión dedicadas.
7. Revisión independiente del rango completo y gate del padre sobre HEAD
   congelado.

Mientras cualquiera de esas condiciones falte, este ADR permanece **propuesto y
condicionado**, y ninguna superficie se presenta como validada en producción.
