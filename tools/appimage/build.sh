#!/bin/bash
# Turn an app under apps/ into an aarch64 AppImage, here on the x86_64 host.
#
# Nothing aarch64 runs on this host, so every step is arch-neutral: cargo cross-builds
# the program in the flipctl-cross container build_deploy.sh already has, a Python
# script lays out the AppDir, and appimagetool's mksquashfs packs it behind the
# aarch64 runtime, which is prepended as a file. The result lands in target/appimage
# and build_deploy.sh --apps pushes it to ~/Apps on the device.
#
# Usage:
#   tools/appimage/build.sh apps/radio      one app, as radio-flipctl-aarch64.AppImage
#   tools/appimage/build.sh --all           every app under apps/
#   tools/appimage/build.sh --check FILE    list a bundle's squashfs and assert its shape
#
# Both upstream tools are pinned by sha256 in tools.lock and fetched once into
# target/appimage/tools. SOURCE_DATE_EPOCH is the commit's time, so two builds of one
# tree give one hash.
set -euo pipefail

HERE=$(cd "$(dirname "$0")/../.." && pwd)
TOOLDIR="$HERE/tools/appimage"
OUT="$HERE/target/appimage"
TOOLS="$OUT/tools"
# The apps' own target directory: another workspace and another feature set from
# flipctl's, sharing only the registry cache.
XT="$HERE/target/cross/apps"
CARGO_HOME_DIR="$HERE/target/cross/home"
TARGET=aarch64-unknown-linux-gnu

usage() { sed -n '2,15p' "$0"; }

# Which command builds an image: BuildKit is what docker means by a builder from 23
# on, and the CLI reaches it through the buildx plugin. Without that plugin `docker
# build` falls back to the classic builder and says so on every run, and
# DOCKER_BUILDKIT=1 fails outright rather than helping. Held rather than streamed: on
# a good build the classic builder's notice is the only thing said, and a bad one has
# to be read in full. Same lines as build_deploy.sh, for the same reason.
docker_image() {
    local tag=$1 dockerfile=$2 context=$3
    local image said
    if docker buildx version >/dev/null 2>&1; then
        image=(docker buildx build --load)
    else
        image=(docker build)
    fi
    if ! said=$("${image[@]}" -q -t "$tag" -f "$dockerfile" "$context" 2>&1 >/dev/null); then
        printf '%s\n' "$said" >&2
        exit 1
    fi
}

# The pinned tools, fetched when missing and checked every time.
fetch_tools() {
    mkdir -p "$TOOLS"
    local name url sum
    while read -r name url sum; do
        case "$name" in ''|'#'*) continue ;; esac
        if ! echo "$sum  $TOOLS/$name" | sha256sum -c --quiet >/dev/null 2>&1; then
            echo "== fetching $name =="
            curl -fsSL -o "$TOOLS/$name.part" "$url"
            mv -f "$TOOLS/$name.part" "$TOOLS/$name"
            echo "$sum  $TOOLS/$name" | sha256sum -c --quiet
        fi
        chmod 755 "$TOOLS/$name"
    done < "$TOOLDIR/tools.lock"
    # uv arrives as a tarball with the binary a directory down, and a bundle wants the
    # binary. Taken out once, beside the tarball the lock checked.
    if [ -f "$TOOLS/uv" ] && [ ! -x "$TOOLS/uv.bin" ]; then
        tar -xzOf "$TOOLS/uv" --wildcards "*/uv" > "$TOOLS/uv.bin"
        chmod 755 "$TOOLS/uv.bin"
    fi
}

# The program, cross-built, and its link surface checked: a framework app needs libc,
# libm and libgcc_s and nothing else, so anything more is a C dependency creeping in.
cross_build() {
    local app=$1 bin=$2
    mkdir -p "$XT" "$CARGO_HOME_DIR"
    echo "== cross-building $bin =="
    docker run --rm -u "$(id -u):$(id -g)" \
        -v "$HERE:/src" -w /src \
        -v "$XT:/target" -e CARGO_TARGET_DIR=/target \
        -v "$CARGO_HOME_DIR:/cargo" -e CARGO_HOME=/cargo \
        flipctl-cross \
        sh -c "cargo build --release --locked --target $TARGET --manifest-path apps/$app/Cargo.toml && \
               aarch64-linux-gnu-readelf -d /target/$TARGET/release/$bin \
                 | awk '/NEEDED/ { gsub(/[][]/, \"\", \$5); print \$5 }' | sort" \
        > "$OUT/$app.needed"
    local want="libc.so.6
libgcc_s.so.1
libm.so.6"
    if [ "$(cat "$OUT/$app.needed")" != "$want" ]; then
        echo "$bin links more than libc, libm and libgcc_s:" >&2
        cat "$OUT/$app.needed" >&2
        exit 1
    fi
}

# In the bundle container, with the repository at /src and the output at /out.
in_bundle() {
    docker run --rm --platform linux/amd64 -u "$(id -u):$(id -g)" \
        -v "$HERE:/src" -w /src -v "$OUT:/out" \
        -e SOURCE_DATE_EPOCH="$(git -C "$HERE" log -1 --format=%ct)" -e ARCH=aarch64 \
        -e TMPDIR=/out/tmp \
        flipctl-bundle "$@"
}

build_app() {
    local dir=$1
    local app bin version staged
    app=$(basename "$dir")
    [ -f "$HERE/apps/$app/app.toml" ] || { echo "no manifest at apps/$app/app.toml" >&2; exit 1; }
    version=$(git -C "$HERE" describe --always --dirty)
    mkdir -p "$OUT/tmp"

    # A crate is built and its binary staged. An app with an AppRun of its own is
    # staged as it stands: that is a runtime bundle, which carries files and pinned
    # tools rather than anything compiled here.
    staged=()
    if [ -f "$HERE/apps/$app/Cargo.toml" ]; then
        bin=$(python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))["package"]["name"])' \
              "$HERE/apps/$app/Cargo.toml")
        cross_build "$app" "$bin"
        staged=(--binary "/src/target/cross/apps/$TARGET/release/$bin")
    fi
    echo "== staging $app =="
    in_bundle python3 tools/appimage/bundle.py stage "apps/$app" "/out/$app/AppDir" \
        "${staged[@]}" --version "$version" --repo /src
    # The name carries flipctl and the architecture: a folder holds AppImages from
    # anywhere, and a person looking at it should be able to tell which are for the
    # panel. What decides that for flipctl is still the manifest inside.
    echo "== packing $app-flipctl-aarch64.AppImage =="
    rm -f "$OUT/$app-flipctl-aarch64.AppImage"
    in_bundle /out/tools/appimagetool --appimage-extract-and-run -n --comp zstd \
        --mksquashfs-opt -Xcompression-level --mksquashfs-opt 19 \
        --runtime-file /out/tools/runtime-aarch64 \
        "/out/$app/AppDir" "/out/$app-flipctl-aarch64.AppImage" 2>&1 | grep -v "^$" | sed 's/^/  /'
    (cd "$OUT" && sha256sum "$app-flipctl-aarch64.AppImage" > "$app-flipctl-aarch64.AppImage.sha256")
    echo "== $(du -h "$OUT/$app-flipctl-aarch64.AppImage" | cut -f1) $OUT/$app-flipctl-aarch64.AppImage =="
}

# The squashfs sits right after the runtime, so its offset is the runtime's size.
check_bundle() {
    local file=$1
    local offset listing
    offset=$(stat -c %s "$TOOLS/runtime-aarch64")
    listing=$(in_bundle unsquashfs -o "$offset" -d "" -ll "/out/$(realpath --relative-to="$OUT" "$file")")
    printf '%s\n' "$listing"
    for want in AppRun app.toml; do
        printf '%s\n' "$listing" | grep -qE "[ /]$want\$" || { echo "no $want at the root" >&2; exit 1; }
    done
}

[ $# -ge 1 ] || { usage; exit 2; }
mkdir -p "$OUT"
docker_image flipctl-cross "$HERE/ci/cross.Dockerfile" "$HERE/ci"
docker_image flipctl-bundle "$HERE/ci/bundle.Dockerfile" "$HERE/ci"
fetch_tools

case "$1" in
    -h|--help) usage ;;
    --all)
        for dir in "$HERE"/apps/*/; do
            build_app "$dir"
        done
        ;;
    --check)
        [ $# -eq 2 ] || { usage; exit 2; }
        check_bundle "$2"
        ;;
    *)
        build_app "$1"
        ;;
esac
