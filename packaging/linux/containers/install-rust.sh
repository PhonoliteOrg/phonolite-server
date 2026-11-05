#!/bin/sh
set -eu

# Run only in the disposable build image, never on the host.
curl --fail --location --proto '=https' --tlsv1.2 https://sh.rustup.rs -o /tmp/phonolite-rustup.sh
sh /tmp/phonolite-rustup.sh -y --profile minimal --default-toolchain "$RUST_TOOLCHAIN"
rm -f /tmp/phonolite-rustup.sh
useradd --create-home --uid 1000 builder
mkdir -p /src /build /output
chown -R builder:builder /opt/cargo /opt/rustup /src /build /output
