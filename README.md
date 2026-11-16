# arl — alternativa MIT a the reference app

**Nombre de trabajo `arl` (provisional).** No es una marca, un nombre de producto aprobado ni un canal de distribución. La decisión de nombre y distribución (D-11) sigue pendiente.

**Estado vigente: / validadas localmente; en implementación parcial, no iniciada. Transporte host, base Android, fuente/reporte Linux y núcleo C SPSC/lifecycle cuentan con verificaciones acotadas. No hay vertical Android→Linux validado ni APK nativo de captura, Keystore real o medición física completa. El lifecycle host tiene revisión independiente y pruebas normal+ASan/UBSan; sus objetos compilan con NDK para arm64/x86_64, no acreditan ejecución Android.** Ver [estado /](docs/plan/execution-f2-f3.md).

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

## Decisiones registradas que afectan al repositorio

- **(x86) — resuelta por the maintainer el 2026-11-13:** objetivos **x86-64** y **ARM64**; **sin soporte de 32 bits**. Los artefactos GNU propuestos son `x86_64-unknown-linux-gnu` y `aarch64-unknown-linux-gnu`.
- **(Android) — congelada en:** no se crea Gradle ni app Android en. minSdk28 es propuesta; target/compile se fijan en y se revalidan en.
- **(baseline) — sin evidencia física:** ningún número de latencia, CPU o memoria de este repositorio es un resultado logrado. El presupuesto de [docs/latency-budget.md](docs/latency-budget.md) es una propuesta.
- **D-11 (nombre/distribución) — pendiente:** `arl` es provisional y no hay canales de release decididos.

## Estructura

```text
README.md este documento
INDEX.md índice raíz único de documentación
AGENTS.md reglas de trabajo del repositorio
LICENSE MIT, Copyright (c) 2026 the maintainer
Cargo.toml workspace Rust (resolver 2, miembros crates/*)
Cargo.lock lock con checksums de
rust-toolchain.toml toolchain fijada a Rust 1.80.1
deny.toml política propuesta de cargo-deny (no ejecutada)
crates/audio-core/ frame v1, PCM16, jitter, métricas, drift, budget
crates/proto/ control estricto y wire (serde/serde_json fijados)
docs/plan/ plan y cobertura (snapshots históricos) + acta de ejecución
docs/security/ modelo de amenaza y flujo de emparejamiento
docs/protocol.md contrato de control y audio
docs/config-schema.md configuración requested/effective y claves locales
docs/dependency-ledger.md deps efectivas, sólo-lock, candidatos y excluidos
docs/latency-budget.md presupuesto por etapa y recursos (propuesta)
contracts/ JSON Schema 2020-12 y catálogo de errores
scripts/check-doc-links.sh checker de enlaces locales (bash, sin dependencias)
scripts/ci-doc.sh gate documental y de core por fase
tools/verify.py verificador documental multi-modo (stdlib)
```

## Verificación de fase 1

```sh
bash scripts/ci-doc.sh --phase f1
python3 tools/verify.py --contracts --docs --phase f1 --licenses --matrix --budget --forbid-insecure-tls --inventory
python3 -m unittest discover -s tools/tests
cargo test --locked --workspace --all-targets
cargo test --locked --workspace --all-targets -- --ignored
```

El gate comprueba enlaces, esquemas, contratos, ledger contra el grafo real de Cargo, presupuesto de audio y de etapas, inventario fail-closed, `cargo fmt --check`, `clippy` con warnings como errores, `cargo doc` con `RUSTDOCFLAGS=-Dwarnings`, tests y el drift de 1 h marcado `#[ignore]`. El workflow de GitHub Actions **no se ha ejecutado** en este proyecto: la evidencia es local. La simulación de drift no es un benchmark ni una medición de hardware.

## Documentos de entrada

- [Índice raíz](INDEX.md)
- [Acta de autoridad y ownership /](docs/plan/execution-f0-f1.md)
- [Acta de fase 01](docs/reviews/fase-01.md)
- [Modelo de amenaza](docs/security/threat-model.md)
- [Emparejamiento](docs/security/pairing.md)
- [Protocolo v1](docs/protocol.md)
- [Configuración](docs/config-schema.md)
- [Presupuesto de latencia](docs/latency-budget.md)
- [Errores](contracts/errors.md)

## Licencia

Código y documentación propios bajo **MIT**; ver [LICENSE](LICENSE). Las dependencias conservan sus licencias y se registran por separado en el [ledger](docs/dependency-ledger.md): usa `serde`, `serde_json`, `itoa`, `memchr` y `ryu`. Ningún componente GPL se copia ni se enlaza de forma accidental; `libpulse`, si se usa, es dinámica y con obligaciones LGPL explícitas.
