ARG BASE_IMAGE=docker.io/library/archlinux:base
FROM ${BASE_IMAGE} AS environment
ARG RUST_TOOLCHAIN=stable
ENV CARGO_HOME=/opt/cargo RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:$PATH CARGO_TARGET_DIR=/build/target
RUN pacman -Syu --noconfirm --needed base-devel cmake perl pkgconf python ca-certificates curl binutils git systemd shadow
COPY install-rust.sh /tmp/phonolite-install-rust.sh
RUN sh /tmp/phonolite-install-rust.sh
USER builder
WORKDIR /src
