# Modelo de amenaza — `arl` v1


Alcance: audio de micrófono Android → Linux por LAN, control bidireccional de configuración y estado, enrolamiento local y sesión autenticada. Quedan fuera: exposición a Internet, captura de audio de otras apps, retorno de audio (fase opt-in) y cualquier API administrativa remota.

## 1. Activos

| ID | Activo | Por qué importa |
|---|---|---|
| A1 | Audio del micrófono Android | Dato personal en tiempo real; su interceptación o inyección es la amenaza central. |
| A2 | Configuración y estado de sesión | Afecta qué se captura, a qué volumen y si el micrófono está activo. |
| A3 | Identidad persistente del daemon y del cliente | Es el ancla de las sesiones siguientes; suplantarla rompe toda la confianza. |
| A4 | Ventana de enrolamiento, pin SPKI y token de un solo uso | Permite provisionar identidad; un token filtrado o reutilizado enrola un dispositivo ajeno. |
| A5 | ACL, capacidades y tickets | Deciden qué puede hacer cada identidad; default deny. |
| A6 | Logs, metadatos y contadores | No deben filtrar token, claves, audio ni identificadores innecesarios. |
| A7 | Socket local de control | Un acceso indebido permite cambiar configuración y parar sesiones. |
| A8 | Artefactos de build y release | Un binario o APK alterado anula el resto de controles. |

## 2. Fronteras de confianza

| ID | Frontera | Cruza |
|---|---|---|
| B1 | App Android ↔ red LAN | Audio, control y mensajes de sesión salen del dispositivo. |
| B2 | LAN ↔ endpoint `enroll` del daemon | Cliente sin certificado, token, prueba de pin. |
| B3 | LAN ↔ endpoint `session` del daemon | Identidad de cliente, audio y control operativo. |
| B4 | CLI ↔ daemon por Unix socket | Operaciones locales autenticadas por UID. |
| B5 | Daemon ↔ PipeWire / SO | Nodo `Audio/Source`, permisos y dispositivos. |
| B6 | Daemon ↔ estado en disco | Identidad, ACL y configuración; sin secretos en claro innecesarios. |

**Regla de frontera:** `enroll` y `session` son **endpoints distintos**, con certificado de servidor distinto y conjuntos de mensajes disjuntos. El pin del QR de enrolamiento **no** es el ancla de las sesiones posteriores. Un mismo canal no puede pasar de enrolamiento a sesión sin una conexión nueva y una identidad provisionada.

## 3. Adversarios considerados

1. Observador pasivo en la LAN (sniffing, metadata).
2. Atacante activo en la LAN (MITM, ARP/ND spoofing, DNS/mDNS falso, replay).
3. App o usuario malicioso en la misma LAN que intenta conectar o floodear.
4. Par emparejado malicioso o comprometido que ya tiene una identidad.
5. Ladrón de un dispositivo Android desbloqueado o de la máquina Linux.
6. Usuario local sin privilegios en la máquina Linux.
7. Atacante de disponibilidad (flood de conexiones, mensajes o datagramas).
8. Observador físico del QR o de la terminal durante el enrolamiento.
9. Dependencia o artefacto de build comprometido.

## 4. Invariantes de seguridad

- **I1.** Nunca se entrega audio antes de que la sesión esté autorizada.
- **I2.** El endpoint de enrolamiento no acepta audio, control operativo ni tickets.
- **I3.** El endpoint de sesión exige identidad de cliente válida: mTLS con `peer_cert` explícito + ACL, o app-layer estándar revisado con binding al canal TLS (decisión /ADR-0002 en). `verify_peer(true)` **no** sustituye la comprobación de `peer_cert`.
- **I4.** No hay TOFU silencioso: una identidad desconocida no se acepta ni se recuerda sin aprobación local explícita.
- **I5.** No existe `verify=false`, `danger_accept_invalid_certs` ni equivalente en builds de release; tampoco fallback automático a UDP claro ni 0-RTT.
- **I6.** El canal remoto no concede permisos Android, no inicia el micrófono y no escribe claves locales.
- **I7.** Todo parser y toda cola tienen límites de tamaño, profundidad y tasa; nada asigna memoria proporcional a la entrada.
- **I8.** Revocar una identidad cierra sus sesiones e invalida sus tickets.

## 5. Amenazas y tratamiento

| ID | Amenaza | Superficie | Mitigación | Riesgo residual |
|---|---|---|---|---|
| T-01 | Sniffing del audio o del control en la LAN | B1/B3 | TLS 1.3/QUIC con cifrado y autenticación; sin UDP claro; sin downgrade | Metadatos de volumen/duración visibles para un observador de red; se acepta en LAN. |
| T-02 | MITM en el enrolamiento para robar el token | B2 | El cliente valida el pin SPKI del certificado efímero **antes** de enviar el token; TTL corto; token de un solo uso; aprobación local con fingerprint y código corto | Usuario que aprueba sin comparar el código; se mitiga con UI y se acepta como error humano. |
| T-03 | Suplantación del daemon en sesión | B3 | Identidad persistente del servidor y anclaje de confianza definido en (CA local o pin de sesión); el cliente rechaza un ancla distinta | Compromiso de la máquina Linux; fuera del modelo. |
| T-04 | Cliente sin certificado aceptado como sesión | B3 | `peer_cert == None` se rechaza antes de control/audio; ACL default deny; test negativo obligatorio en | Implementación futura podría malinterpretar `verify_peer`; el gate lo prueba. |
| T-05 | TOFU: aceptar y recordar una identidad no aprobada | B2/B3 | Sin alta automática; identidad sólo tras aprobación local; estado perdido ⇒ re-enrolar, nunca aceptar un servidor/cliente nuevo en silencio | Usuario que re-enrola sin verificar; se registra en el acta. |
| T-06 | Replay o reutilización del token de enrolamiento | B2 | Token aleatorio de 256 bits, un solo uso, TTL ≤120 s, ligado a la ventana; se invalida al aprobar, cancelar o expirar; no se registra en logs | Captura física del QR durante la ventana; TTL y confirmación local lo acotan. |
| T-07 | Audio o control inyectados en el endpoint de enrolamiento | B2 | El servidor de enrolamiento sólo habla mensajes de enrolamiento; audio/control/ticket se rechazan con error y cierre; el receptor de audio descarta todo datagrama fuera de sesión autorizada | Un bug de implementación; cubierto por `enrollment_boundary` en. |
| T-08 | App maliciosa en la LAN intenta conectar a `session` | B3 | Identidad de cliente obligatoria + ACL; rate limit por IP/identidad; rechazo sin filtrar si la identidad existe | Fuerza bruta de recursos; se acota con cuotas y se monitoriza. |
| T-09 | Par emparejado malicioso pide más de lo permitido | B3 | Capacidades y ACL por operación; claves locales rechazadas; sin shell, archivos, HTTP ni elevación; el micrófono lo inicia sólo el usuario Android | El par puede saturar su propia cuota de control; se limita y se corta la sesión. |
| T-10 | Ladrón usa un dispositivo ya emparejado | B1/B3 | Revocación por identidad cierra sesión e invalida tickets; tickets de vida corta; re-enrolamiento tras revocar; bloqueo del dispositivo es responsabilidad de Android | Ventana hasta que el usuario revoca; se documenta. |
| T-11 | Usuario local sin privilegios controla el daemon | B4 | Socket en `$XDG_RUNTIME_DIR/arl/control.sock`, directorio `0700`, socket `0600`, `SO_PEERCRED` con UID propietario; sin TCP administrativo | Procesos del mismo UID; se acepta (mismo usuario). |
| T-12 | Flood de conexiones, mensajes o datagramas | B2/B3 | Límites antes y después del handshake: conexiones por IP, tasa de intentos, tamaño máximo, profundidad, strings, frames por datagrama, timeouts y cuotas por identidad; colas acotadas con descarte | DoS volumétrico en la red o el host; fuera del control de la aplicación. |
| T-13 | Parser malicioso provoca pánico, OOM o consumo cuadrático | B2/B3 | Límites exactos de [`docs/protocol.md`](../protocol.md); rechazo sin asignación proporcional; fuzz de control y datagrama en | Bug desconocido; el fuzz es requisito de, no de. |
| T-14 | Fuga de token, claves o audio en logs | A6/B6 | Token nunca persistido ni emitido por `--json`; logs sin secretos; material de identidad en estado protegido; revisión en | Logs de depuración de terceros; se prohíbe en código y se revisa. |
| T-15 | QR observado por un tercero cercano | B2 | `arl pair open` sólo en TTY, TTL corto, token de un solo uso, aprobación local; el QR no se imprime con `--json` ni se escribe en disco | Cámara en el momento exacto; el código corto y la aprobación local lo acotan. |
| T-16 | Dependencia o artefacto de build comprometido | A8 | Ledger con pin/licencia/procedencia; builds reproducibles y SBOM; sin binarios opacos; revisión de licencias | Compromiso upstream no detectado; se reduce con pin y auditoría, no se elimina. |
| T-17 | Micrófono activo sin conocimiento del usuario | B1/B5 | FGS con notificación y parada local; inicio desde UI visible; muerte de proceso no deja micrófono huérfano; el remoto no puede iniciarlo | Bug de OEM/OS; se prueba en API28/34/37 en. |
| T-18 | Nodo PipeWire expuesto tras terminar la sesión | B5 | El nodo deja de producir al parar; no se publica Audio/Source sin sesión activa; parada local y por `session stop` | Nodo visible vacío durante la transición; se acepta y se documenta. |

## 6. Riesgos residuales aceptados

Se aceptan explícitamente, con revisión en el gate:

1. **Aprobación humana errónea** en el enrolamiento (comparar fingerprint/código es responsabilidad del usuario).
2. **DoS volumétrico** a nivel de red/host, fuera del alcance de la aplicación.
3. **Compromiso del SO o del dispositivo** ya emparejado; la revocación es la respuesta, no la prevención.
4. **Bugs de OEM Android** en FGS, micrófono o Keystore; se prueban dispositivos concretos en y la matriz.
5. **Metadatos de red** (IP, puertos, volumen de tráfico, duración de sesión).
6. **Incertidumbre de licencia de pesos RNNoise** y de artefactos DFN; no se distribuyen hasta resolverla.
7. **Limitación de Keystore** si no hay bridge de firma: se declarará en ADR-0002 y no se prometerá clave HW no exportable. **No se inventa un esquema criptográfico en.**

## 7. Supuestos y límites

- La LAN se considera hostil; el modelo **no** asume una red doméstica confiable.
- El daemon Linux no se expone a Internet. Si el usuario lo hace, queda fuera de soporte y debe documentarse como riesgo propio.
- El audio es PCM16 mono 48 kHz en v1; el contrato exacto está en [`docs/protocol.md`](../protocol.md).
- La identidad de cliente, el handshake de enrolamiento, el formato exacto de los mensajes de pairing y el almacenamiento de claves son **diseño pendiente de **; este documento sólo fija requisitos y prohibiciones.
- No hay auditoría de implementación: no tiene código que auditar. Los tests negativos citados (`tls_identity_gate`, `enrollment_boundary`, `pairing_flow`) se ejecutan en.

## 8. Pendientes que bloquean el gate

| Pendiente | Dueño | Criterio de cierre |
|---|---|---|
| Elección mTLS vs app-layer con binding TLS (/ADR-0002) | | Cliente sin identidad rechazado; `peer_cert` probado; sin cripto propia. |
| Anclaje de confianza del servidor de sesión (CA local o pin de sesión) | | Cliente rechaza un ancla distinta; sin `verify=false`. |
| Formato exacto de QR y mensajes de enrolamiento | | Token de un solo uso, TTL, pin validado antes del token, audio/control rechazados. |
| Límites numéricos de flood y cuotas por identidad | | Valores medidos y probados, no sólo declarados aquí. |
| Bridge Keystore o limitación declarada | | ADR-0002 con decisión y límite explícito. |
