# Licencias — política del proyecto

**Own code and docs are MIT** ([LICENSE](../LICENSE)). This document sets the policy for third parties; it does not approve any dependency. The [ledger](dependency-ledger.md) records candidates and pins.

## Política

| Clase | Regla |
|---|---|
| Propio | MIT. Sin copiar código de terceros al árbol propio. |
| Permisivas (MIT, Apache-2.0, BSD-2/3, ISC, Zlib, MPL-2.0) | Aceptables como candidatas si el ledger registra pin, origen y rol; avisos preservados. |
| LGPL | Sólo `libpulse`, **enlace dinámico** contra la biblioteca del sistema. Nunca estático ni vendorizado. |
| GPL / AGPL | **Excluido.** No se copia, no se enlaza, no se añade al build. |
| Pesos de modelos | Se tratan como artefactos aparte del código; sin licencia explícita, **no se distribuyen**. |
| Sin licencia / incierta | No se distribuye ni se incorpora. |

## Obligaciones por caso

- **MIT / BSD / ISC / Zlib:** conservar copyright y texto de licencia del artefacto distribuido; no se reescribe la licencia del tercero.
- **Apache-2.0 (Oboe, si lo adopta):** conservar `LICENSE`/`NOTICE` del artefacto y declarar cambios propios; no usar marcas del proyecto upstream.
- **LGPL-2.1-or-later (libpulse):** enlace dinámico, aviso de la biblioteca y su licencia en la documentación de distribución, posibilidad de sustituir la biblioteca (relink) y oferta de fuente de las modificaciones de la biblioteca —no previstas—. El software propio sigue MIT; el binario combinado no se relicencia.
- **NDK / C / C++ (quiche-BoringSSL, Oboe, Opus, SpeexDSP):** la licencia se audita sobre el artefacto y sus transitivas en, no sólo sobre el repositorio upstream. Un build con NDK no cambia la licencia del código propio.
- **Patentes:** la licencia de código no resuelve por sí sola una revisión de patentes; no se afirma cobertura de patentes para Opus, WebRTC ni BoringSSL.

## Pesos RNNoise

El código RNNoise v0.2 está declarado BSD-3-Clause; los pesos `rnnoise_data-0b50c45` **no tienen licencia explícita observada dentro del tarball**. Es una incertidumbre documental, no un dictamen de ilegalidad. Mientras no se resuelva procedencia y licencia: **los pesos no se distribuyen, no se vendorizan y no se empaquetan** en ningún artefacto. El código puede evaluarse sin ellos sólo si el build no los incorpora.

## GPL

Quedan excluidos del árbol, del build y del enlace: `snd-aloop` (GPL-2.0-or-later), la opción `gpl-fftw3` de SpeexDSP y cualquier componente GPL/AGPL. La compatibilidad opt-in con ALSA se documenta, no se copia. `libpulse` es LGPL, no GPL, y se usa dinámica.

## Estado en

- No hay dependencias efectivas, ni `NOTICE`, ni código de terceros en el repositorio.
- No se ejecutó `cargo deny`, `cargo tree` ni auditoría de artefactos: no existe producto en y declararlo sería falso.
- La auditoría de licencias de artefactos concretos y transitivas es; la app Android y el NDK se auditan en.
- Este documento es política; el cumplimiento global de SC-001 sigue **pendiente** hasta la auditoría real.
