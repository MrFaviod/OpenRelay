# Licenses — project policy

**The project's own code and documentation are MIT** ([LICENSE](../LICENSE)). This document sets the policy for third parties; it does not approve any dependency.

## Policy

| Class | Rule |
|---|---|
| Own | MIT. No third-party code copied into our own tree. |
| Permissive (MIT, Apache-2.0, BSD-2/3, ISC, Zlib, MPL-2.0) | Acceptable as candidates if pin, origin and role are recorded; notices preserved. |
| LGPL | Only `libpulse`, **dynamically linked** against the system library. Never static or vendored. |
| GPL / AGPL | **Excluded.** Not copied, not linked, not added to the build. |
| Model weights | Treated as artifacts separate from the code; without an explicit license, **not distributed**. |
| Unlicensed / uncertain | Not distributed or incorporated. |

## Obligations by case

- **MIT / BSD / ISC / Zlib:** keep the copyright and license text of the distributed artifact; a third party's license is not rewritten.
- **Apache-2.0 (Oboe):** keep upstream LICENSE and NOTICE.
- **LGPL-2.1-or-later (libpulse):** dynamic linking, a notice of the library and its license in the distribution documentation, the ability to substitute the library (relink) and an offer of the source for library modifications — none planned. Our own software remains MIT; the combined binary is not relicensed.
- **NDK / C / C++ (quiche-BoringSSL, Oboe, Opus, SpeexDSP):** the license is audited on the artifact and its transitives, not only on the upstream repository. A build with the NDK does not change the license of our own code.
- **Patents:** a code license does not by itself settle a patent review; no patent coverage is claimed for Opus, WebRTC or BoringSSL.

## RNNoise weights

The RNNoise v0.2 code is declared BSD-3-Clause; the `rnnoise_data-0b50c45` weights **have no explicit license observed inside the tarball**. This is a documentary uncertainty, not a finding of illegality. Until provenance and license are resolved: **the weights are not distributed, not vendored and not packaged** in any artifact. The code can be evaluated without them only if the build does not incorporate them.

## GPL

Excluded from the tree, the build and linking: `snd-aloop` (GPL-2.0-or-later), SpeexDSP's `gpl-fftw3` option and any GPL/AGPL component. Opt-in compatibility with ALSA is documented, not copied. `libpulse` is LGPL, not GPL, and is used dynamically.

## Status

- The license audit is done on the built artifact and its transitives, not only on the upstream repository.
- The Android app and the NDK are audited with the release artifact.
- This document is policy; it does not replace that audit.
