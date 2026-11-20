# Retorno de audio Linux→Android

**Estado: implementado en el daemon bajo la feature `return-audio`, apagado por defecto.** La lógica vive en `crates/daemon/src/return_audio.rs`, la captura del monitor en `crates/audio-linux/src/monitor.rs` y la puerta en el runtime del daemon.

## Qué es

El retorno es el audio que el daemon Linux envía al dispositivo Android por la dirección inversa a la captura del micrófono. Reutiliza el frame `type=1` de [`protocol.md`](protocol.md) §8 sin cambios: PCM16LE, 48 kHz, mono, 240 muestras y 480 bytes de payload. No hay perfil nuevo, códec nuevo ni mensaje de control nuevo.

## Opt-in y apagado

Está **apagado por defecto** y exige dos condiciones a la vez:

1. **Feature en ambos extremos.** Android anuncia `audio.return-pcm16-48k-mono` en `capabilities` y el daemon sólo la cuenta como local **si `audio.return_enabled = true`**. Si falta en cualquiera, no hay retorno.
2. **Clave local del daemon.** `audio.return_enabled = true` en la configuración del daemon (ver [`config-schema.md`](config-schema.md)); su default es `false`.

Las features locales efectivas salen de la config, no de una constante: con la clave apagada el daemon no se anuncia el retorno a sí mismo ni abre el flujo. El daemon todavía no envía un mensaje `capabilities` propio al peer; cuando lo haga, deberá anunciar exactamente esa lista efectiva.

Si falta cualquiera de las dos, el daemon **no envía ni un datagrama** de retorno: no hay negociación implícita ni fallback silencioso.

## Un solo flujo de retorno a la vez

El daemon usa **un único `MonitorSource`** (un solo stream de captura del sink por defecto de PipeWire) y sirve el retorno a **una sola sesión a la vez**. Si dos peers anuncian el retorno, se atiende la primera sesión autorizada; las demás no reciben retorno hasta que esa cierre. Repartir un único anillo de captura entre varios peers dejaría a todos con huecos, así que no se hace. La secuencia `seq` sólo avanza cuando el envío del frame tuvo éxito: un fallo de transporte no consume número ni abre un hueco falso.

Para apagarlo:

- Pon `audio.return_enabled = false` (o elimina la clave, que ya vale `false` por defecto).
- El daemon se compila con la feature de Cargo `return-audio`, **apagada por defecto**; un binario sin ella no puede emitir retorno aunque la config lo pida.

En v0.2.1 el retorno **no está disponible en el producto**: `config set` responde
`unsupported` para cualquier clave (no hay aplicador), la app no anuncia la
feature y el CLI no concede la capacidad ACL `return_audio`. Lo descrito aquí es
el diseño del crate, no una función que se pueda activar hoy.

## Aviso de eco

Antes de activar el retorno, el extremo Android debe mostrar un aviso de eco/feedback: el audio que suena en el dispositivo puede volver a entrar por su micrófono y realimentarse. Sin ese aviso, el retorno no se activa. El aviso es responsabilidad del cliente Android; el contrato no lo sustituye.

## ACL `return_audio`: pending

El transporte exige la capacidad ACL `return_audio` para `send_audio` (además de las dos condiciones de opt-in). Hoy **no existe un camino de producto que la conceda**: `pairing::approve` sabe persistir el flag `return_audio` en el archivo de identidades y `pairing::load` lo relee, pero el CLI `openrelay` no expone ningún comando de emparejamiento/aprobación que lo ponga a `true`. Queda en **pendiente**: hasta que exista ese camino, una identidad aprobada no puede recibir retorno y el daemon no debe considerarlo disponible en producción. No se inventa aquí un mensaje ni un flag nuevo; concederlo requiere llamar a `pairing::approve` con `Capabilities::with_return_audio` desde el flujo de aprobación que se implemente.

## Fuera de alcance

- **Captura de apps Android.** El retorno no captura ni reproduce audio de otras apps del dispositivo; sólo transporta el flujo que el daemon Linux produce.
- **Mezcla con el micrófono.** El retorno es un flujo independiente, con su propia `epoch` y `seq`. Nunca se mezcla en el mismo datagrama ni en la misma época que el audio Android→Linux.
- **Formato distinto de PCM16LE 48 kHz mono.** No se negocia remuestreo ni otro perfil en v1.
