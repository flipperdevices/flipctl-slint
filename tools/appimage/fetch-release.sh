#!/bin/sh
# Unpack an app's pinned upstream release files into the bundle it ships as.
#
# Usage:
#   tools/appimage/fetch-release.sh apps/<app>
#
# An app with a `release.lock` carries programs nobody packages for Debian, taken
# from their projects' own release pages. Each line names where a file goes in the
# bundle, the url it comes from, and that url's sha256. A tarball url gives up the
# member named like the destination; anything else is the file itself. What comes
# out is `<app>/runtime`, laid out as the AppDir is, which the staging copies over
# the bundle's own root so `usr/bin` is the one AppRun already puts on PATH. That is
# also the shape fetch-debs.sh unpacks a deb.lock into, so an app with both gets
# both: build.sh runs that one first, and these files are added to what it left.
#
# Downloads are cached in target/releases by checksum and verified every run, so a
# second build fetches nothing and a tampered cache is caught rather than used.
set -eu

HERE=$(cd "$(dirname "$0")/../.." && pwd)
[ $# -eq 1 ] || { sed -n '2,6p' "$0"; exit 2; }
APP=$(cd "$1" && pwd)
LOCK="$APP/release.lock"
[ -f "$LOCK" ] || { echo "fetch-release: no $LOCK" >&2; exit 1; }

CACHE="$HERE/target/releases"
DEST="$APP/runtime"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$CACHE"

count=0
while read -r path url sha; do
    case "$path" in ''|'#'*) continue ;; esac
    file="$CACHE/$sha"
    if [ ! -f "$file" ] || [ "$(sha256sum "$file" | cut -d' ' -f1)" != "$sha" ]; then
        curl -fsSL --retry 3 -o "$file.part" "$url"
        got=$(sha256sum "$file.part" | cut -d' ' -f1)
        [ "$got" = "$sha" ] || { echo "fetch-release: $url is $got, not $sha" >&2; exit 1; }
        mv "$file.part" "$file"
    fi
    mkdir -p "$WORK/$(dirname "$path")"
    case "$url" in
        *.tar.gz|*.tgz)
            name=$(basename "$path")
            member=$(tar -tzf "$file" | grep -E "(^|/)$name\$" | head -1)
            [ -n "$member" ] || { echo "fetch-release: no $name in $url" >&2; exit 1; }
            tar -xzOf "$file" "$member" > "$WORK/$path"
            ;;
        *) cp "$file" "$WORK/$path" ;;
    esac
    case "$path" in
        usr/bin/*) chmod 755 "$WORK/$path" ;;
        *) chmod 644 "$WORK/$path" ;;
    esac
    count=$((count + 1))
done <"$LOCK"

if [ -f "$APP/deb.lock" ]; then
    mkdir -p "${DEST:?}"
    cp -a "$WORK/." "${DEST:?}/"
else
    rm -rf "${DEST:?}"
    mv "$WORK" "${DEST:?}"
    chmod 755 "${DEST:?}"
    trap - EXIT
fi
echo "fetch-release: $count files, $(du -sh "${DEST:?}" | cut -f1) in $DEST"
