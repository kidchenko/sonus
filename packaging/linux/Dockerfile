# A Linux to build and try Sonus in, from a Mac or anywhere else:
#
#   docker build -t sonus-linux packaging/linux
#   docker run --rm -v "$PWD":/src -v sonus-target:/target sonus-linux cargo test
#   docker run --rm -v "$PWD":/src -v sonus-target:/target sonus-linux ./build.sh --package
#   docker run --rm -v "$PWD":/src -v sonus-target:/target sonus-linux packaging/linux/test-live.sh
#
# The -dev packages are what the build links against, the same ones CI
# installs. PulseAudio, Xvfb and D-Bus are for running it headless: a sound
# server with a null sink to play test tones into, and a display to draw on.
# `--build-arg UBUNTU=22.04` builds where releases are built.
ARG UBUNTU=24.04
FROM ubuntu:${UBUNTU}

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl pkg-config bc file rpm \
        libgtk-3-dev libayatana-appindicator3-dev libpulse-dev \
        pulseaudio pulseaudio-utils xvfb xauth dbus-x11 \
        fonts-cantarell fonts-dejavu-core python3 python3-gi gir1.2-glib-2.0 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH CARGO_TARGET_DIR=/target
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
        --default-toolchain stable --component clippy,rustfmt

WORKDIR /src
