# packaging

Artefactos para ejecutar `openrelay` como servicio de usuario. Nada de este
directorio se instala solo: son archivos para copiar a mano y una unidad
opcional, no un paquete del sistema.

| Ruta | Qué es |
|---|---|
| `systemd/openrelay.service` | Unidad `systemd --user` opcional; ejecuta `openrelay daemon`. |
| `../scripts/package.sh` | Construye y valida un tarball reproducible con `--check`. |

La unidad usa el binario en `~.local/bin/openrelay` y el socket por defecto
`$XDG_RUNTIME_DIR/openrelay/control.sock`. No abre puertos administrativos, no
necesita root y no modifica el sistema: corre con los privilegios del usuario
que la activa. La instalación y desinstalación manuales están en
[../docs/systemd.md](../docs/systemd.md).

## Tarball

`bash scripts/package.sh --check` compila el binario, arma en un directorio
temporal:

```
openrelay-<versión>
  openrelay 0755
  openrelay.service 0644
  README.md 0644
```

y verifica permisos (sin setuid/setgid, nada fuera del prefijo), que
`openrelay service print-unit` coincida byte por byte con `systemd/openrelay.service` y
que dos ejecuciones produzcan el mismo `sha256`. No instala nada ni escribe
fuera del directorio temporal; al terminar lo borra.

El script no fija `LD_LIBRARY_PATH` ni conoce rutas del host.
