# ADR-0001 — Transporte QUIC para audio (DATAGRAM) y control (stream fiable)

- **Estado:** Accepted: QUIC via quinn.
- **Fecha:** 2026-11-14.
- **Relacionado:** [ADR-0002](0002-identity.md), [protocolo v1](../protocol.md), [emparejamiento](../security/pairing.md).

## 1. Contexto

El contrato v1 fija las superficies y deja el motor abierto: el audio viaja como
un frame PCM16 por DATAGRAM y el control por un stream fiable con framing
`u32` big-endian + JSON UTF-8, con los límites de
[`docs/protocol.md`](../protocol.md) (§9) sin redefinir.

Tres restricciones mandan sobre la elección:

1. La toolchain está fijada a **Rust 1.80.1** en `rust-toolchain.toml`.
2. La identidad debe poder firmar con una clave **no exportable** de Android,
   lo que exige un gancho de firma externa en el stack TLS.
3. El crate debe ser reutilizable en Linux y Android sin arrastrar un runtime
   asíncrono ni HTTP/3, porque el bucle de audio lo gobierna el llamador.

## 2. Decisión

Se adopta **`quinn-proto` 0.11.12** con **`rustls` 0.23.22** y **`ring` 0.17.8**,
en modo **sans-IO**: `openrelay-transport` recibe y devuelve datagramas UDP y no abre
sockets ni hilos. El crate se declara en
[`crates/transport/Cargo.toml`](../../crates/transport/Cargo.toml):

| Dependencia | Pin | Papel |
|---|---|---|
| `quinn-proto` | `=0.11.12`, `default-features = false`, feature `rustls-ring` | Motor QUIC (endpoint, conexión, DATAGRAM, streams) |
| `rustls` | `=0.23.22`, features `std`, `ring` | TLS 1.3, verificadores y `SigningKey` externo |
| `ring` | `=0.17.8` | Primitivas criptográficas |
| `rustls-pki-types` | `=1.16.0` | Tipos de certificado y clave |
| `x509-cert` | `=0.2.5` | Parser DER estándar para localizar el SPKI |
| `bytes` | `=1.8.0` | Buffers sin copia del motor |

Se usa el **crate `quinn-proto`**, no el crate `quinn` 0.11.12: este último no
existe en esa versión y el primero es el que expone la API sans-IO con
`peer_identity()`, `handshake_data()` y `export_keying_material()`.

### 2.1 Reparto de superficies

- **Audio:** DATAGRAM QUIC, un `AudioFrame` v1 por datagrama, sin fragmentación
  en este nivel. El datagrama de contrato son 508 B (cabecera 28 B + 480 B de
  payload) y el máximo aceptado es 1200 B
  ([`crates/audio-core/src/frame.rs`](../../crates/audio-core/src/frame.rs)).
  Un datagrama mayor cierra la conexión con código `0x101` en lugar de
  truncarse.
- **Control:** stream bidireccional fiable con `u32` BE de longitud + JSON, con
  `MAX_CONTROL_FRAME = 16388` (16384 de payload + 4 de longitud) tomado de
  `openrelay-proto`, no redeclarado aquí.
- **ALPN:** `openrelay-enroll/1` y `openrelay-session/1` son constantes separadas en
  [`crates/transport/src/lib.rs`](../../crates/transport/src/lib.rs). El
  servidor de sesión rechaza con `RejectReason::Alpn` cualquier handshake que no
  negocie `openrelay-session/1`, y esa comprobación ocurre **antes** de mirar la
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
  `has_0rtt()` y `accepted_0rtt()` para que la prueba afirme que nunca ocurre.
- **Tickets de sesión:** `send_tls13_tickets = 0`.
- **HTTP/3 y Tokio:** no hay dependencia ni tarea de fondo; el `Endpoint` se
  bombea con `poll_transmit`/`poll_timeout`/`handle_timeout` desde el llamador.

## 3. Consecuencias

- El socket de producción se cablea fuera del crate **sin cambiar esta
  frontera**: el crate no conoce `std::net::UdpSocket` ni JNI.
- El bucle de audio decide cuándo bombear, lo que permite un test determinista
  con enlace en memoria y, más adelante, un integrador en el hilo de red de
  Android.
- Al ser sans-IO, ninguna prueba de este crate acredita red real, latencia ni
  comportamiento de socket.
- La API pública (`SessionServer`, `SessionClient`, `SessionLimits`,
  `Capabilities`, `Acl`, `RejectReason`, `ServerEvent`, `ClientEvent`,
  `TransportError`) queda disponible para los consumidores del daemon.

## 4. Alternativas rechazadas

| Alternativa | Motivo del rechazo |
|---|---|
| **quiche 0.30.1** | Declara `rust-version = 1.88`, por encima de la toolchain fijada 1.80.1; y su única carga de clave privada es `Config::load_priv_key_from_pem_file`, sin trait de firma externa. Incompatible con una clave de Keystore no exportable. |
| **quiche 0.22.0** | Compila en 1.80, pero mantiene la misma limitación: sólo clave desde fichero PEM. Descartado por identidad, no por motor. |
| **quinn-proto 0.11.19** | Declara MSRV 1.85, fuera de la toolchain. |
| **crate `quinn` con Tokio** | Añade un runtime asíncrono que el contrato de audio no necesita y complica el callback de tiempo real. |
| **HTTP/3** | No hay HTTP en el protocolo; añade superficie y dependencias sin requisito. |
| **SRTP + DTLS** | No cubre el stream de control con el framing de contrato. |
| **UDP claro con framing propio** | Prohibido por los invariantes de seguridad (sin fallback a UDP claro). |

## 5. Riesgos conocidos

- **Motivo de rechazo impreciso en audio malformado:** en
  [`quinn.rs`](../../crates/transport/src/quinn.rs), un `AudioFrame::decode`
  fallido emite `ServerEvent::Rejected` con `RejectReason::Tls`, un motivo que
  corresponde a otra capa. No cambia el cierre fail-closed (código `0x102`),
  pero confunde la telemetría; corregirlo exige ampliar `RejectReason`, lo que
  es un cambio de API.
- **Sin medición:** ninguna cifra de latencia, CPU o memoria de este ADR es una
  medición.

## 6. Lo que este ADR no decide

- Identidad, mTLS, Keystore y confianza del cliente: [ADR-0002](0002-identity.md).
- Descubrimiento mDNS y puertos.
- Códec (Opus), DSP y métricas de latencia.
- Formato exacto de QR y mensajes de emparejamiento.
