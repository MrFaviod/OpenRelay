# arl — alternativa MIT a the reference app

**Nombre de trabajo `arl` (provisional).** No es una marca, un nombre de producto aprobado ni un canal de distribución. La decisión de nombre y distribución (D-11) sigue pendiente.


## Qué es

Un proyecto de software libre bajo licencia MIT, pensado como alternativa a the reference app para **Linux y Android**:

- Linux en Rust, CLI ligera, sin GUI de escritorio; puede correr en foreground o como unidad `systemd --user` opcional.
- Android nativo como panel de control y origen del micrófono.
- Flujo principal de audio: **micrófono Android → Linux**, por LAN, priorizando latencia sobre calidad.
- Reducción de ruido OSS opcional e integrada (fase posterior; ninguna biblioteca elegida).
- Comunicación de control **bidireccional**: CLI y panel comparten configuración y estado; el audio de retorno Linux→Android es una fase separada, opt-in y apagada por defecto.

## Qué no es (alcance y límites)

- **No** es audio bidireccional en el flujo principal. «Bidireccional» aquí significa control.
- **No** captura audio de otras apps Android; eso queda fuera salvo aprobación explícita y plan propio.
- **No** garantiza compatibilidad universal con cualquier distro, driver, app o dispositivo. La compatibilidad se expresa como matriz con celdas verificadas, limitadas o pendientes.
- **No** usa nube, cuenta, telemetría ni servicio externo.
- **No** abre API administrativa por LAN, shell remoto ni HTTP de control.
- **No** usa `verify=false` ni equivalentes; no hay fallback a UDP claro.

## Decisiones registradas que afectan el repositorio

- **(x86) — resuelta por the maintainer el 2026-11-13:** objetivos **x86-64** y **ARM64**; **sin soporte de 32 bits**. Los artefactos GNU propuestos son `x86_64-unknown-linux-gnu` y `aarch64-unknown-linux-gnu`. musl/Alpine queda como artefacto separado sólo si se valida.
- **(Android) — congelada en:** no se crea Gradle ni app Android en. minSdk28 es propuesta; target/compile se fijan en y se revalidan en.
- **(baseline) — sin evidencia física:** ningún número de latencia, CPU o memoria de este repositorio es un resultado logrado.
- **D-11 (nombre/distribución) — pendiente:** `arl` es provisional y no hay canales de release decididos.

## Estructura

```text
README.md este documento
INDEX.md índice raíz único de documentación
AGENTS.md reglas de trabajo del repositorio
LICENSE MIT, Copyright (c) 2026 the maintainer
docs/plan/ plan y cobertura (snapshots históricos) + acta de ejecución
docs/security/ modelo de amenaza y flujo de emparejamiento
docs/protocol.md contrato de control y audio
docs/config-schema.md configuración requested/effective y claves locales
contracts/ JSON Schema 2020-12 y catálogo de errores
scripts/check-doc-links.sh checker de enlaces locales (bash, sin dependencias)
```

## Verificación documental de

```sh
bash scripts/check-doc-links.sh
bash scripts/check-doc-links.sh --self-test
python3 -m json.tool contracts/control-v1.schema.json >/dev/null
python3 -m json.tool contracts/audio-frame-v1.schema.json >/dev/null
python3 -m json.tool contracts/capabilities-v1.schema.json >/dev/null
find. \(-name Cargo.toml -o -name build.gradle.kts -o -name gradlew \) -print
```

El último `find` debe salir vacío en. Los comandos de producto (cargo, Gradle, pruebas de audio) **no existen todavía** y se declaran propuestos en el plan.

## Documentos de entrada

- [Índice raíz](INDEX.md)
- [Acta de autoridad y ownership /](docs/plan/execution-f0-f1.md)
- [Modelo de amenaza](docs/security/threat-model.md)
- [Emparejamiento](docs/security/pairing.md)
- [Protocolo v1](docs/protocol.md)
- [Configuración](docs/config-schema.md)
- [Errores](contracts/errors.md)

## Licencia

Código y documentación propios bajo **MIT**; ver [LICENSE](LICENSE). Las dependencias conservan sus licencias y se registran por separado. Ningún componente GPL se copia ni se enlaza de forma accidental; `libpulse`, si se usa, es dinámica y con obligaciones LGPL explícitas.
