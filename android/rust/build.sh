#!/usr/bin/env bash
# Compila libopenrelay_bridge.so para arm64-v8a y x86_64 con el NDK 27.2 y la deja en
# android/rust/target/jniLibs/<abi>/ (la consume app/build.gradle.kts).
# Uso: ./build.sh   (NDK y RUST_TOOLCHAIN sobrescribibles por entorno)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
NDK="${NDK:-${ANDROID_NDK_HOME:-$HOME/Android/Sdk/ndk/27.2.12479018}}"
[ -d "$NDK" ] || { echo "NDK no encontrado: $NDK (exporta NDK=/ruta/al/ndk)" >&2; exit 1; }
TC="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
TOOLCHAIN="${RUST_TOOLCHAIN:-1.80.1}"
CARGO=(rustup run "$TOOLCHAIN" cargo)
export RUSTUP_TOOLCHAIN="$TOOLCHAIN"
# libopus (audiopus_sys) declara cmake_minimum_required < 3.5; CMake 4 lo rechaza.
export CMAKE_POLICY_VERSION_MINIMUM=3.5
# cmake-rs usa el toolchain file del NDK si encuentra ANDROID_NDK; CMake del SDK.
export ANDROID_NDK="$NDK" ANDROID_NDK_HOME="$NDK"
SDK_CMAKE="$(dirname "$(dirname "$NDK")")/cmake/3.22.1/bin/cmake"
[ -x "$SDK_CMAKE" ] && export CMAKE="$SDK_CMAKE"

build_abi() {
    local triple="$1" abi="$2" api_triple="$3"
    local cc="$TC/${api_triple}28-clang"
    local env_triple="${triple//-/_}"
    local upper="${env_triple^^}"
    export "CC_${env_triple}=$cc" "CXX_${env_triple}=$cc++" "AR_${env_triple}=$TC/llvm-ar"
    export "CARGO_TARGET_${upper}_LINKER=$cc"
    export "CARGO_TARGET_${upper}_RUSTFLAGS=-C link-arg=-Wl,-z,max-page-size=16384"
    (cd "$here" && "${CARGO[@]}" build --release --locked --target "$triple")
    mkdir -p "$here/target/jniLibs/$abi"
    cp "$here/target/$triple/release/libopenrelay_bridge.so" "$here/target/jniLibs/$abi/"
    echo "OK $abi -> target/jniLibs/$abi/libopenrelay_bridge.so"
}

build_abi aarch64-linux-android arm64-v8a aarch64-linux-android
build_abi x86_64-linux-android x86_64 x86_64-linux-android
