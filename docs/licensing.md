# Licencias — política del proyecto

**El código y la documentación propios son MIT** ([LICENSE](../LICENSE)). Este documento fija la política para terceros; no aprueba ninguna dependencia.

## Política

| Clase | Regla |
|---|---|
| Propio | MIT. Sin copiar código de terceros al árbol propio. |
| Permisivas (MIT, Apache-2.0, BSD-2/3, ISC, Zlib, MPL-2.0) | Aceptables como candidatas si se registran pin, origen y rol; avisos preservados. |
| LGPL | Sólo `libpulse`, **enlace dinámico** contra la biblioteca del sistema. Nunca estático ni vendorizado. |
| GPL / AGPL | **Excluido.** No se copia, no se enlaza, no se añade al build. |
| Pesos de modelos | Se tratan como artefactos aparte del código; sin licencia explícita, **no se distribuyen**. |
| Sin licencia / incierta | No se distribuye ni se incorpora. |

## Obligaciones por caso

- **MIT / BSD / ISC / Zlib:** conservar copyright y texto de licencia del artefacto distribuido; no se reescribe la licencia del tercero.
- **Apache-2.0 (Oboe):** keep upstream LICENSE and NOTICE.
- **LGPL-2.1-or-later (libpulse):** enlace dinámico, aviso de la biblioteca y su licencia en la documentación de distribución, posibilidad de sustituir la biblioteca (relink) y oferta de fuente de las modificaciones de la biblioteca —no previstas—. El software propio sigue MIT; el binario combinado no se relicencia.
- **NDK / C / C++ (quiche-BoringSSL, Oboe, Opus, SpeexDSP):** la licencia se audita sobre el artefacto y sus transitivas, no sólo sobre el repositorio upstream. Un build con NDK no cambia la licencia del código propio.
- **Patentes:** la licencia de código no resuelve por sí sola una revisión de patentes; no se afirma cobertura de patentes para Opus, WebRTC ni BoringSSL.

## Pesos RNNoise

El código RNNoise v0.2 está declarado BSD-3-Clause; los pesos `rnnoise_data-0b50c45` **no tienen licencia explícita observada dentro del tarball**. Es una incertidumbre documental, no un dictamen de ilegalidad. Mientras no se resuelva procedencia y licencia: **los pesos no se distribuyen, no se vendorizan y no se empaquetan** en ningún artefacto. El código puede evaluarse sin ellos sólo si el build no los incorpora.

## GPL

Quedan excluidos del árbol, del build y del enlace: `snd-aloop` (GPL-2.0-or-later), la opción `gpl-fftw3` de SpeexDSP y cualquier componente GPL/AGPL. La compatibilidad opt-in con ALSA se documenta, no se copia. `libpulse` es LGPL, no GPL, y se usa dinámica.

## Estado

- La auditoría de licencias se hace sobre el artefacto construido y sus
  transitivas, no sólo sobre el repositorio upstream.
- La app Android y el NDK se auditan con el artefacto de release.
- Este documento es política; no sustituye esa auditoría.
