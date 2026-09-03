# Cross-build flipctl for the Flipper without an aarch64 machine, for
# ./build_deploy.sh --cross.
#
# A cross needs a linker for the libc it links against and nothing else, because
# nothing in flipctl links a C library except libc: EGL and the GPU loader are
# dlopened through libloading, wayland-client uses its own Rust backend rather than
# libwayland, drm-rs is ioctls against pre-generated bindings rather than libdrm, and
# i-slint-common dlopens fontconfig. Checked on the binary this produces, whose only
# NEEDED entries are libc, libm and libgcc_s.
#
# The image's own rustc has to satisfy slint 1.17's 1.92 minimum, which every current
# release does. What it links against is Debian's aarch64 glibc, so the binary asks
# for symbols up to GLIBC_2.39 and the device's trixie provides 2.41.
FROM rust:latest

RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
 && rm -rf /var/lib/apt/lists/*
RUN rustup target add aarch64-unknown-linux-gnu

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
