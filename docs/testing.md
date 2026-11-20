# Pruebas extremo a extremo (audio real)

Dos pruebas cubren el camino completo: el arnés sintético `scripts/e2e/run.sh` y
una corrida de la app Android real sobre un emulador. Sólo lo que ambas (o una, con
su alcance declarado) observaron se presenta como probado; el resto queda en
[Qué no está probado](#qué-no-está-probado).

`scripts/e2e/run.sh` levanta un contenedor `ubuntu:24.04` (arm64) con una
sesión D-Bus, PipeWire, WirePlumber y pipewire-pulse corriendo como el usuario
sin privilegios `relay`, compila OpenRelay, lo instala con `install.sh` desde un
tarball local, empareja un teléfono sintético y verifica que el tono de 440 Hz
llegue a `openrelay-mic` por PipeWire y por PulseAudio. Termina con
`E2E: PASS` y sale con código 0 sólo si todas las comprobaciones pasan.

## Requisitos

- Docker con soporte arm64 y `--network host` (el contenedor abre UDP 47000 y
  47001 y habla con el daemon por `127.0.0.1`).
- No hace falta PipeWire en el host: todo corre dentro del contenedor.
- La primera ejecución compila en release dentro del volumen
  `openrelay-e2e-target`; las siguientes reutilizan la caché.

## Uso

```sh
scripts/e2e/run.sh
```

Variables útiles: `IMAGE` (tag de la imagen, por defecto
`openrelay-e2e:24.04`), `TARGET_VOLUME` (volumen de compilación) y
`KEEP_IMAGE=1` (no borra la imagen al terminar). Los artefactos quedan en
`scripts/e2e/artifacts/` (ignorado por git): `console.log`, `analysis.txt`
`pulse-analysis.txt`, `wpctl.txt`, `pactl-sources.txt`, `nodes-streaming.txt`
`status-*.json`, `devices-*.txt`, `fake-phone*.log` y los WAV capturados.

## Qué comprueba

1. D-Bus de sesión, PipeWire, WirePlumber y pipewire-pulse vivos; `wpctl` y
   `pactl` responden.
2. `cargo build --locked --release` de la CLI y del ejemplo `fake_phone`, y
   `install.sh` instalando un tarball construido localmente
   (`OPENRELAY_BASE_URL=file://…`).
3. `openrelay daemon`; `openrelay pair --advertise 127.0.0.1 --yes` aprobado
   automáticamente; `fake_phone` se enrola con la URI, abre la sesión TLS
   (`serverName openrelay`) y emite un seno de 440 Hz en PCM16LE mono 48 kHz
   tramas de 240 muestras, a tiempo real.
4. `pw-cli ls Node` publica `openrelay-mic` con `media.class = "Audio/Source"`;
   `pactl list short sources` lo lista en `s16le 1ch 48000Hz`.
5. `pw-record --target openrelay-mic` y `parec -d openrelay-mic` capturan
   varios segundos; `analyze.py` exige RMS ≥ 0,01 (≈ −40 dBFS) y pico
   dominante en 440 ± 5 Hz, e informa el RMS en dBFS.
6. `openrelay devices remove <fingerprint>` borra la identidad de
   `identities.json`, la saca de la ACL viva y cierra su sesión; una reconexión
   con la identidad guardada se rechaza en la autorización
   (`unauthorized`) y el estado final queda con 0 sesiones vivas.

## Arreglos del daemon que cubre

- El cliente sintético usaba época 0, que `FrameHeader` rechaza; ahora usa
  época 1.
- El harness arrancaba los procesos de fondo con un `shift` incorrecto y sin
  entorno XDG; ahora los arranca con D-Bus y directorios XDG.
- `openrelay status` contaba sesiones autorizadas acumuladas en vez de sesiones
  vivas; ahora informa las vivas y `devices remove` se refleja de inmediato.
- `install.sh` acepta `OPENRELAY_BASE_URL` para instalar desde un directorio
  local (`file://…`) sin tocar el código de descarga.

## App real en un emulador (Android 16 arm64)

La app Android real (APK debug) corrió en un contenedor Android con Android 16
arm64. El micrófono se alimentó por un HAL simulado desde un FIFO con un tono de
440 Hz. El emparejamiento se hizo pegando la URI `openrelay://pair` (no con la
cámara). Resultados observados:

- `pw-record` desde `openrelay-mic`: RMS −21.08 dBFS, tono en ~440 Hz
  (autocorrelación 440.37 Hz); `parec` desde la misma fuente: RMS −21.72 dBFS.
- `audio.stats`: `pcm16_frames` creciendo durante el flujo.
- `openrelay devices remove <fingerprint>`: la app mostró
  «Sin conexión: sesión cerrada antes de autorizar» y no volvió a conectar.
- `kill` del daemon y reinicio: la app reconectó sola en ~2 s y el audio siguió.
- Arreglo asociado (commit `8a02ce8`): la app ahora informa sesiones cerradas o
  rechazadas en vez de quedarse mostrando un enlace muerto.

## Qué no está probado

Estas capacidades no tienen evidencia de ejecución y no deben presentarse como
funcionales en la documentación:

- Escaneo de QR con cámara física (sólo se pegó la URI; el escaneo de cámara no
  se ejercitó en un teléfono real).
- Teléfono físico por Wi-Fi (la corrida fue en un emulador en el mismo host).
- Latencia extremo a extremo, batería y consumo.
- Varios teléfonos transmitiendo a la vez.
- La unidad `systemd --user` ejecutando el daemon bajo una sesión de escritorio
  real.
- `audio.gain_db`: la app guarda el valor pero ninguna muestra se escala.
- Opus: el decodificador existe y se prueba, pero la app no lo negocia; el flujo
  real es PCM16.

## Última ejecución PASS

```text
PASS: D-Bus session bus on unix:path=/tmp/dbus-LHE5LYE4AD,guid=dd2a10f671265560905b50d96b002259
PASS: PipeWire daemon is up (pid 23)
PASS: WirePlumber session manager is running (pid 30)
PASS: pipewire-pulse answers pactl (PulseAudio API)
PASS: install.sh installed /home/relay.local/bin/openrelay from local tarball
PASS: fake phone enrolled and session authorized
PASS: openrelay-mic media.class is Audio/Source
PASS: pactl lists openrelay-mic: 48	openrelay-mic	PipeWire	s16le 1ch 48000Hz	RUNNING
PASS: RMS=-15.05 dBFS (linear 0.1768, >=0.01) peak=440.01 Hz (+0.01 Hz vs 440 Hz)
PASS: RMS=-15.05 dBFS (linear 0.1768, >=0.01) peak=439.99 Hz (-0.01 Hz vs 440 Hz)
PASS: removing the device closed its live session (status sessions 0)
REFUSED: el servidor rechazó la sesión en la autorización (unauthorized)
PASS: removed identity is rejected at session authorization (connect refused/unauthorized)
E2E: PASS
```
