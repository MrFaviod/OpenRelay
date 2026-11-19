# packaging

Artefactos para ejecutar `arl` como servicio de usuario. Nada de este
directorio se instala solo: son archivos para copiar a mano y una unidad
opcional, no un paquete del sistema.

| Ruta | Qué es |
|---|---|
| `systemd/arl.service` | Unidad `systemd --user` opcional; ejecuta `arl daemon`. |
| `../scripts/package.sh` | Construye y valida un tarball reproducible con `--check`. |

La unidad usa el binario en `~.local/bin/arl` y el socket por defecto
`$XDG_RUNTIME_DIR/arl/control.sock`. No abre puertos administrativos, no
necesita root y no modifica el sistema: corre con los privilegios del usuario
que la activa. La instalación y desinstalación manuales están en
[../docs/systemd.md](../docs/systemd.md).

## Tarball

`bash scripts/package.sh --check` compila el binario, arma en un directorio
temporal:

```
arl-<versión>
  arl 0755
  arl.service 0644
  README.md 0644
```

y verifica permisos (sin setuid/setgid, nada fuera del prefijo), que
`arl service print-unit` coincida byte por byte con `systemd/arl.service` y
que dos ejecuciones produzcan el mismo `sha256`. No instala nada ni escribe
fuera del directorio temporal; al terminar lo borra.

## Bibliotecas en hosts con PipeWire staged

Antes de comparar la unidad, `package.sh --check` ejecuta `arl service
print-unit` desde el tarball temporal. Si el binario no carga sus bibliotecas
el script termina con el error del cargador (por ejemplo, `libpipewire-0.3.so.0:
cannot open shared object file`), no con un «no coincide».

En un host con PipeWire instalado en el sistema no hace falta nada. Si se usa
un PipeWire staged fuera de las rutas del cargador, exporta `LD_LIBRARY_PATH`
a su directorio de bibliotecas antes de ejecutar el script:

```sh
export LD_LIBRARY_PATH=/ruta/al/pipewire/root/usr/lib/<triple>$LD_LIBRARY_PATH
bash scripts/package.sh --check
```

El script no fija `LD_LIBRARY_PATH` ni conoce rutas del host.
