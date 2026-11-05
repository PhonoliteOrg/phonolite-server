ARG BASE_IMAGE=docker.io/library/almalinux:9
FROM ${BASE_IMAGE} AS environment
ARG RUST_TOOLCHAIN=stable
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH CARGO_TARGET_DIR=/build/target
RUN dnf -y upgrade --refresh && dnf -y install --allowerasing \
    gcc gcc-c++ make cmake perl pkgconf-pkg-config python3 ca-certificates curl \
    rpm-build redhat-rpm-config binutils findutils file tar xz shadow-utils \
    && dnf clean all
COPY install-rust.sh /tmp/phonolite-install-rust.sh
RUN sh /tmp/phonolite-install-rust.sh
USER builder
WORKDIR /src
