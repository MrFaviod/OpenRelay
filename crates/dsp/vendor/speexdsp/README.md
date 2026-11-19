# SpeexDSP 1.2.1 (subconjunto vendorizado)

Origen: tarball oficial `https://downloads.xiph.org/releases/speex/speexdsp-1.2.1.tar.gz`,
sha256 `8c777343e4a6399569c72abc38a95b24db56882c83dbdb6c6424a5f4aeb54d3d`.
Licencia: **BSD-3-Clause** (ver `COPYING`). No se modifican los fuentes.

## Por qué está aquí

No existe un crate de crates.io que compile SpeexDSP desde fuente:
`speexdsp-sys` 0.1.x exige `pkg-config` y una `libspeexdsp` del sistema (no
instalada en estas máquinas) y su feature `build` busca un directorio
`speexdsp/` que el paquete no incluye. Por eso `openrelay-dsp` compila este
subconjunto con `cc` sólo cuando la feature `speexdsp` está activa.

## Qué se incluye

Sólo lo que necesita `speex_preprocess_state_init` / `speex_preprocess_run`:

- `libspeexdsp/preprocess.c`: supresor de ruido.
- `libspeexdsp/fftwrap.c` + `libspeexdsp/smallft.c`: FFT **smallft** integrada
  (BSD-3-Clause). No se compila `kiss_fft` ni se activa FFTW; la opción
  `gpl-fftw3` queda excluida.
- `libspeexdsp/filterbank.c`: banco de filtros del preprocesador.
- `libspeexdsp/mdf.c`: sólo aporta `speex_echo_get_residual`, que
  `preprocess.c` referencia al enlazar aunque el AEC no se use.
- Cabeceras `speex/speexdsp_types.h`, `speex/speex_preprocess.h` y
  `speex/speex_echo.h`, más las internas que incluyen.

`include/speex/speexdsp_config_types.h` no viene en el tarball: lo genera
`configure` desde `speexdsp_config_types.h.in`. Aquí se fija la variante
`<stdint.h>` que produce autotools en Linux/Android/macOS.

No se incluyen `resample.c`, `jitter.c`, `buffer.c`, `scal.c` ni las pruebas:
`openrelay-dsp` no los usa.
