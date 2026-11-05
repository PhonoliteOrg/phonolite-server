ARG BASE_IMAGE=docker.io/library/ubuntu:22.04
FROM ${BASE_IMAGE} AS environment
ARG RUST_TOOLCHAIN=stable
ENV DEBIAN_FRONTEND=noninteractive CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH CARGO_TARGET_DIR=/build/target
RUN apt-get update && apt-get -y dist-upgrade && apt-get install -y --no-install-recommends \
    build-essential cmake perl pkg-config python3 python3-yaml ca-certificates curl dpkg-dev binutils passwd xz-utils \
    && rm -rf /var/lib/apt/lists/*
COPY install-rust.sh /tmp/phonolite-install-rust.sh
RUN sh /tmp/phonolite-install-rust.sh
USER builder
WORKDIR /src
