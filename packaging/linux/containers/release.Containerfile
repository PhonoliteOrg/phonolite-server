ARG BUILD_IMAGE
FROM ${BUILD_IMAGE} AS builder
COPY --chown=1000:1000 . /src
# Rust's executables and toolchains remain in /opt; Cargo's downloads and build
# products start empty for each release. No persistent cache mounts are used.
ENV CARGO_HOME=/build/release-cargo CARGO_TARGET_DIR=/build/release-target
ARG BUILD_IMAGE
ARG BUILD_ENVIRONMENT_ID
ENV PHONOLITE_BUILD_ENVIRONMENT=${BUILD_IMAGE}
ENV PHONOLITE_BUILD_ENVIRONMENT_ID=${BUILD_ENVIRONMENT_ID}
ARG BASELINE
ARG PACKAGE_FORMAT
ARG RUST_TARGET
ARG PACKAGE_MAINTAINER=Phonolite
ARG SOURCE_DATE_EPOCH
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
RUN python3 -m unittest discover -s scripts/tests -p test_package_baselines.py -v
RUN if [ "$PACKAGE_FORMAT" = deb ]; then \
    python3 -m unittest discover -s scripts/tests -p test_package_linux.py -v; fi
RUN test ! -e "$CARGO_HOME" && test ! -e "$CARGO_TARGET_DIR" \
    && cargo fetch --locked --target "$RUST_TARGET" \
    && python3 scripts/package-linux.py --native --format "$PACKAGE_FORMAT" --baseline "$BASELINE" \
        --target "$RUST_TARGET" --output-dir /output --maintainer "$PACKAGE_MAINTAINER"
FROM scratch
COPY --from=builder /output /output
