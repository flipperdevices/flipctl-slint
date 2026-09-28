#!/bin/sh
# Unpack an app's pinned Debian packages into the bundle it ships as.
#
# Usage:
#   tools/appimage/fetch-debs.sh apps/<app>
#
# An app with a `deb.lock` carries its whole userspace rather than naming packages
# for the device to install. What comes out is `<app>/runtime`, a small /usr that
# the app's AppRun points its loader at, which is why nothing here needs apt, root,
# or an aarch64 host: a .deb is an ar archive of a tarball, and bsdtar reads both.
#
# Downloads are cached in target/debs and verified every run, so a second build
# fetches nothing and a tampered cache is caught rather than used.
#
# glibc is the one thing not carried. A program cannot bring its own loader: the
# kernel runs the ld-linux the binary names, and that one has to be the device's, so
# libc and libm have to be the device's too. Everything else is ours, including the
# graphics and audio libraries, which only need to load here rather than to work:
# the engine draws in software and the bundle never makes a GL context.
set -eu

HERE=$(cd "$(dirname "$0")/../.." && pwd)
[ $# -eq 1 ] || { sed -n '2,6p' "$0"; exit 2; }
APP=$(cd "$1" && pwd)
LOCK="$APP/deb.lock"
[ -f "$LOCK" ] || { echo "fetch-debs: no $LOCK" >&2; exit 1; }

CACHE="$HERE/target/debs"
DEST="$APP/runtime"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$CACHE"

# The device's own, and the loader that has to match them.
EXCLUDE="ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 librt.so.1"

count=0
while read -r url sha size; do
    case "$url" in ''|'#'*) continue ;; esac
    # The pool url percent-encodes what a version string cannot spell, and the cached
    # file is named for the url so two versions never collide.
    file="$CACHE/$(basename "$url")"
    if [ ! -f "$file" ] || [ "$(sha256sum "$file" | cut -d' ' -f1)" != "$sha" ]; then
        curl -fsSL --retry 3 -o "$file.part" "$url"
        got=$(sha256sum "$file.part" | cut -d' ' -f1)
        [ "$got" = "$sha" ] || { echo "fetch-debs: $(basename "$url") is $got, not $sha" >&2; exit 1; }
        mv "$file.part" "$file"
    fi
    [ "$(stat -c %s "$file")" = "$size" ] || { echo "fetch-debs: $(basename "$url") is the wrong size" >&2; exit 1; }
    # An ar archive holding data.tar.{xz,zst,gz}: libarchive picks the one it finds
    # and decompresses it without being told which.
    bsdtar -xOf "$file" 'data.tar*' | bsdtar -xf - -C "$WORK"
    count=$((count + 1))
done <"$LOCK"

# Debian is usr-merged, but a package may still lay itself down under /lib.
if [ -d "${WORK:?}/lib" ]; then
    mkdir -p "${WORK:?}/usr/lib"
    cp -a "${WORK:?}/lib/." "${WORK:?}/usr/lib/"
    rm -rf "${WORK:?}/lib"
fi

for lib in $EXCLUDE; do
    rm -f "${WORK:?}/usr/lib/aarch64-linux-gnu/$lib"
done

# Only the first game is played, and the second is another 29 MB of it.
rm -f "${WORK:?}/usr/share/games/doom/freedoom2.wad"

# Documentation goes, except the copyright files: this bundle redistributes eighty
# packages and their terms have to travel with them.
find "${WORK:?}/usr/share/doc" -type f ! -name copyright -delete 2>/dev/null || true
# A package whose docs are a symlink to another package's now points at nothing, and
# a dangling link is what makes the staging copy fail rather than the bundle broken.
find "${WORK:?}/usr/share/doc" -xtype l -delete 2>/dev/null || true
find "${WORK:?}/usr/share/doc" -type d -empty -delete 2>/dev/null || true
rm -rf "${WORK:?}/usr/share/man" "${WORK:?}/usr/share/locale" "${WORK:?}/usr/share/lintian" \
    "${WORK:?}/usr/share/bug" "${WORK:?}/usr/share/applications" "${WORK:?}/usr/share/icons" \
    "${WORK:?}/usr/share/menu" "${WORK:?}/usr/share/metainfo" "${WORK:?}/usr/share/pkgconfig" \
    "${WORK:?}/etc" "${WORK:?}/usr/share/alsa" "${WORK:?}/usr/share/gcc" \
    "${WORK:?}/usr/share/gdb" "${WORK:?}/usr/share/doc-base" \
    "${WORK:?}/usr/share/bash-completion" "${WORK:?}/usr/share/glib-2.0"

# The engine's package also builds Heretic and Hexen, which need game data this
# bundle does not carry and could not play without it.
rm -f "${WORK:?}/usr/games/dsda-heretic" "${WORK:?}/usr/games/dsda-hexen" \
    "${WORK:?}/usr/games/freedoom1" "${WORK:?}/usr/games/freedoom2"

rm -rf "${DEST:?}"
mkdir -p "$(dirname "${DEST:?}")"
mv "$WORK" "${DEST:?}"
trap - EXIT
echo "fetch-debs: $count packages, $(du -sh "${DEST:?}" | cut -f1) in $DEST"
