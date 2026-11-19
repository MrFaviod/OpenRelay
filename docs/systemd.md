# Servicio de usuario con systemd

Unidad **opcional** para dejar `openrelay daemon` corriendo bajo `systemd --user`.
No es un paquete del sistema ni un servicio privilegiado: corre con el UID que
la activa, no abre puertos administrativos, no instala archivos por sí sola y
no requiere root. El daemon acepta conexiones iniciadas desde Android (socket
Unix local para la CLI y, si se aprovisiona, QUIC entrante); nunca conecta
hacia afuera.

El archivo canónico es [../packaging/systemd/openrelay.service](../packaging/systemd/openrelay.service).
`openrelay service print-unit` imprime su contenido exacto, y
`bash scripts/package.sh --check` comprueba que ambos coincidan.

## Requisitos

- Sesión `systemd --user` activa (`systemctl --user status`).
- Binario en `~.local/bin/openrelay` (la unidad usa `%h.local/bin/openrelay`).
- `XDG_RUNTIME_DIR` válido: systemd lo define para la sesión del usuario y el
  daemon crea ahí `openrelay/control.sock` (directorio 0700, socket 0600).
- Bibliotecas del binario resolubles por el cargador. Si `openrelay` enlaza un
  PipeWire staged (no instalado en el sistema), exporta `LD_LIBRARY_PATH` al
  directorio que contiene `libpipewire-0.3.so.0` antes de ejecutar el binario
  `package.sh` o la unidad. La unidad no fija esa variable; para el servicio
  añade un drop-in con `Environment=LD_LIBRARY_PATH=...`.

## Instalar y activar a mano

```sh
install -d ~.local/bin
install -m 0755 <ruta-al-binario>/openrelay ~.local/bin/openrelay

install -d ~.config/systemd/user
install -m 0644 packaging/systemd/openrelay.service ~.config/systemd/user/openrelay.service

systemctl --user daemon-reload
systemctl --user enable --now openrelay.service
systemctl --user status openrelay.service
```

Comprobación funcional y parada:

```sh
openrelay status
systemctl --user stop openrelay.service
```

Los mensajes del proceso van al journal de usuario:

```sh
journalctl --user -u openrelay.service -e
```

`enable` crea el enlace en `default.target`, así que la unidad arranca con la
sesión del usuario. Para que sobreviva al cierre de sesión haría falta
`loginctl enable-linger`, que es una decisión de política del sistema y puede
requerir al administrador; esta unidad no lo activa.

## Equivalencia con primer plano

`openrelay daemon` ya es un proceso de primer plano: no tiene flag `--foreground`
ni se bifurca. La unidad no cambia el comportamiento del binario, sólo lo
supervisa:

| Primer plano | Unidad |
|---|---|
| `openrelay daemon` en una terminal | `ExecStart=%h.local/bin/openrelay daemon` |
| Ctrl-C para detenerlo | `systemctl --user stop openrelay.service` |
| socket `$XDG_RUNTIME_DIR/openrelay/control.sock` | el mismo, heredado de systemd |
| sale con 0 al recibir SIGTERM | `Restart=on-failure` no lo reinicia |

`Restart=on-failure` reinicia el proceso si falla, no tras una parada limpia.
No se pasan `--socket` ni `--quic`: los valores por defecto son los mismos que
en primer plano. Si se necesita QUIC, se edita la unidad para añadir los cinco
archivos de provisión; siguen siendo conexiones entrantes.

## Hardening y por qué no rompe el arranque

- `NoNewPrivileges=yes`: el daemon no eleva privilegios.
- `ProtectSystem=strict`: `/usr`, `/etc`, `/boot` y el resto del sistema quedan
  de sólo lectura. `ReadWritePaths=%t` devuelve escritura sólo al directorio de
  ejecución del usuario, que es donde el daemon crea `openrelay/control.sock`. El
  estado de control vive en memoria; no hay otro archivo que escribir.
- `PrivateTmp=yes`: `/tmp` privado, sin rutas compartidas entre servicios.
- `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`: AF_UNIX para el socket
  local; AF_INET/AF_INET6 para el endpoint QUIC si se aprovisiona.
- `ProtectKernelTunables`, `ProtectControlGroups`, `RestrictNamespaces`
  `RestrictSUIDSGID`, `LockPersonality`, `SystemCallArchitectures=native` y
  `UMask=0077` cierran superficie sin tocar el camino de arranque.

El daemon no necesita `$HOME` para arrancar: sin `--quic` no lee archivos de
identidad. Si se añaden a la unidad rutas bajo `$HOME`, siguen siendo lecturas
y `ProtectSystem=strict` no las bloquea. Si se cambiara el socket a una ruta
fuera de `%t`, habría que añadirla a `ReadWritePaths`.

## Desinstalar

```sh
systemctl --user disable --now openrelay.service
rm -f ~.config/systemd/user/openrelay.service
systemctl --user daemon-reload
```

El binario y el directorio `$XDG_RUNTIME_DIR/openrelay` se borran a mano si no se
usan; el gestor no los toca.

## Verificar sin instalar

```sh
systemd-analyze --user verify packaging/systemd/openrelay.service
bash scripts/package.sh --check
```
