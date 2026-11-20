# Reproducible desktop-Linux test bed for OpenRelay's PipeWire source.
#
#   docker build -t openrelay-e2e:24.04 -f scripts/e2e/Dockerfile .
#
# Ubuntu 24.04 ships PipeWire 1.0.5 and WirePlumber, which is what a normal
# desktop user runs. The image contains a full Rust toolchain so the daemon is
# built from the checked-out source exactly as a contributor would build it.
FROM ubuntu:24.04

ARG RUST_VERSION=1.80.1
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config clang libclang-dev \
        libpipewire-0.3-dev libopus-dev \
        pipewire pipewire-bin wireplumber pipewire-pulse \
        pulseaudio-utils dbus dbus-user-session \
        python3 python3-numpy \
        curl ca-certificates git tar gzip procps \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/bash relay
USER relay
ENV HOME=/home/relay \
    CARGO_HOME=/home/relay/.cargo \
    RUSTUP_HOME=/home/relay/.rustup \
    PATH=/home/relay/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION"

WORKDIR /src
