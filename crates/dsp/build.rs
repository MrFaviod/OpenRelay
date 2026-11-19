//! Compila el subconjunto vendorizado de SpeexDSP 1.2.1.
//!
//! Sólo hace algo con la feature `speexdsp`; sin ella el crate no compila C ni
//! enlaza `libm`. La FFT es `smallft` (BSD-3-Clause): no se define
//! `USE_KISS_FFT` ni se activa FFTW.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/speexdsp");
    if std::env::var_os("CARGO_FEATURE_SPEEXDSP").is_some() {
        build_speexdsp();
    }
}

fn build_speexdsp() {
    let root = std::path::Path::new("vendor/speexdsp");
    let mut build = cc::Build::new();
    build
        .include(root.join("include"))
        .include(root.join("include/speex"))
        .include(root.join("libspeexdsp"))
        .define("FLOATING_POINT", None)
        .define("USE_SMALLFT", None)
        .define("EXPORT", Some(""))
        .warnings(false)
        .file(root.join("libspeexdsp/preprocess.c"))
        .file(root.join("libspeexdsp/fftwrap.c"))
        .file(root.join("libspeexdsp/filterbank.c"))
        .file(root.join("libspeexdsp/smallft.c"))
        .file(root.join("libspeexdsp/mdf.c"));
    build.compile("speexdsp_ns");

    // smallft usa cos/sin/sqrt de libm.
    println!("cargo:rustc-link-lib=m");
}
