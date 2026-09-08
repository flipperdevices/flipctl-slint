# Package an app as an aarch64 AppImage without an aarch64 machine, for
# tools/appimage/build.sh.
#
# Nothing here runs aarch64 code: the AppDir is staged by a Python script,
# appimagetool's mksquashfs packs it, and the aarch64 runtime is prepended as a file.
# desktop-file-utils and file because appimagetool refuses to run without either.
#
# Pinned by the manifest list's digest, and amd64 by name: this host has no qemu, and
# the local debian:trixie tag has been seen holding an arm64 variant that cannot run.
FROM --platform=linux/amd64 debian:trixie@sha256:f324c7ff54321e8d9c588493a20244965938ce0aa50bbd1022d38010e9ffc4b1

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        python3 desktop-file-utils squashfs-tools file curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
