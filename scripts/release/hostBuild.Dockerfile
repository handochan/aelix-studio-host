# Linux release compiler for aioncore (ported from Aelix Studio's
# scripts/studio-preview/packaging/hostBuild.Dockerfile; the Node stage that
# Studio used for its own resource preparation is not needed here).
# Debian 11 keeps the binary's GLIBC requirement within the upstream release
# ceilings (x64 2.34, ARM64 2.30); build.py checks each ELF after the build.
# The base image and apt snapshot are pinned; build.py binds this file's
# SHA-256 into the image label and the build receipt.
FROM docker.io/library/rust:1.95.0-bullseye@sha256:28afaeb8445f2a2e7d878bd34ed39ba02bb517efb29986188cbd59b7cf4f2fdf
RUN printf '%s\n' \
    'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20260801T000000Z/ bullseye main' \
    'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20260801T000000Z/ bullseye-security main' \
    > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/* \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends clang libclang-dev cmake \
    && rm -rf /var/lib/apt/lists/* \
    && chmod -R a+rwX /usr/local/cargo /usr/local/rustup
ARG HOST_BUILD_RECIPE_SHA
LABEL org.aelix.host-build.recipe=${HOST_BUILD_RECIPE_SHA}
