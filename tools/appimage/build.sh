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
#   tools/appimage/build.sh --native ...    the same, using the host's own tools
#
# --native is for a build host that has the toolchain but no docker, which is what
# the image build is: the containers only carry a cross toolchain and squashfs-tools,
# and a host that already cross-builds flipctl has most of that. It says what is
# missing rather than failing in the middle. Everything else is identical, including
# the output, because nothing here was ever architecture-specific.
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
# Whether the containers are used, and what the repository and the output are called
# from inside whatever runs the steps: the container mounts them, native mode is
# already standing in them.
NATIVE=0
SRCDIR=/src
OUTDIR=/out
# appimagetool runs where the build runs, so it is the builder's architecture. The
# container is amd64 by name, so that path always takes the x86_64 one.
TOOL=appimagetool-x86_64
# Where cargo leaves the binary. A build for aarch64 on an aarch64 builder is not a
# cross and must not be told it is one: --target there makes cargo build every proc
# macro and build script twice, and the linker it would want does not exist under
# that name on a machine whose own gcc is already the right one.
XTARGET_ARG=(--target "$TARGET")
RELDIR=$TARGET/release

usage() { sed -n '2,17p' "$0"; }

# What native mode needs, named the way build-flipctl.sh names what it needs: a build
# that is going to stop should stop before it has built anything.
preflight_native() {
    # No squashfs-tools: appimagetool carries its own mksquashfs and is run with
    # --appimage-extract-and-run, which is also what saves the host needing FUSE.
    # unsquashfs is wanted by --check alone and asked for there.
    local missing=()
    command -v python3 >/dev/null || missing+=("python3")
    command -v desktop-file-validate >/dev/null || missing+=("desktop-file-utils")
    command -v file >/dev/null || missing+=("file")
    command -v curl >/dev/null || missing+=("curl")
    if [ ${#missing[@]} -gt 0 ]; then
        echo "build.sh --native: missing ${missing[*]}" >&2
        echo "build.sh --native: on Debian, apt-get install ${missing[*]}" >&2
        exit 1
    fi
}

# What a crate app needs on top of that, asked for where it is used: a runtime bundle
# carries files rather than anything compiled, and stages perfectly well on a host
# with no cross toolchain at all.
preflight_cross() {
    local libdir
    # Already the target: the host toolchain is the whole answer.
    if [ "$(uname -m)" = aarch64 ]; then
        return
    fi
    # --print target-libdir computes a path and says nothing about whether the target
    # is installed there, so this looks for the library rather than the directory.
    libdir=$(rustc --print target-libdir --target "$TARGET" 2>/dev/null || true)
    if [ -z "$libdir" ] || ! compgen -G "$libdir/libstd-*.rlib" >/dev/null; then
        echo "build.sh --native: no Rust std for $TARGET" >&2
        echo "build.sh --native: rustup target add $TARGET, or apt-get install libstd-rust-dev:arm64" >&2
        exit 1
    fi
    if ! command -v aarch64-linux-gnu-gcc >/dev/null; then
        echo "build.sh --native: no linker for $TARGET" >&2
        echo "build.sh --native: on Debian, apt-get install gcc-aarch64-linux-gnu libc6-dev-arm64-cross" >&2
        exit 1
    fi
}

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
    # readelf reads any architecture, so the check is the same either way; the
    # container's is prefixed only because that is what its binutils installs.
    if [ "$NATIVE" = 1 ]; then
        preflight_cross
        CARGO_TARGET_DIR="$XT" \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
            cargo build --release --locked "${XTARGET_ARG[@]}" \
                --manifest-path "$HERE/apps/$app/Cargo.toml"
        readelf -d "$XT/$RELDIR/$bin" \
            | awk '/NEEDED/ { gsub(/[][]/, "", $5); print $5 }' | sort > "$OUT/$app.needed"
    else
        docker run --rm -u "$(id -u):$(id -g)" \
            -v "$HERE:/src" -w /src \
            -v "$XT:/target" -e CARGO_TARGET_DIR=/target \
            -v "$CARGO_HOME_DIR:/cargo" -e CARGO_HOME=/cargo \
            flipctl-cross \
            sh -c "cargo build --release --locked --target $TARGET --manifest-path apps/$app/Cargo.toml && \
                   aarch64-linux-gnu-readelf -d /target/$TARGET/release/$bin \
                     | awk '/NEEDED/ { gsub(/[][]/, \"\", \$5); print \$5 }' | sort" \
            > "$OUT/$app.needed"
    fi
    local want="libc.so.6
libgcc_s.so.1
libm.so.6"
    if [ "$(cat "$OUT/$app.needed")" != "$want" ]; then
        echo "$bin links more than libc, libm and libgcc_s:" >&2
        cat "$OUT/$app.needed" >&2
        exit 1
    fi
}

# One staging or packing step, with the repository at $SRCDIR and the output at
# $OUTDIR. In the container those are mounts; natively they are where they already
# are, and the environment is the same either way so the bundles are too.
in_bundle() {
    if [ "$NATIVE" = 1 ]; then
        env SOURCE_DATE_EPOCH="$(git -C "$HERE" log -1 --format=%ct)" ARCH=aarch64 \
            TMPDIR="$OUT/tmp" "$@"
    else
        docker run --rm --platform linux/amd64 -u "$(id -u):$(id -g)" \
            -v "$HERE:/src" -w /src -v "$OUT:/out" \
            -e SOURCE_DATE_EPOCH="$(git -C "$HERE" log -1 --format=%ct)" -e ARCH=aarch64 \
            -e TMPDIR=/out/tmp \
            flipctl-bundle "$@"
    fi
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
        staged=(--binary "$SRCDIR/target/cross/apps/$RELDIR/$bin")
    fi
    echo "== staging $app =="
    in_bundle python3 "$SRCDIR/tools/appimage/bundle.py" stage "apps/$app" "$OUTDIR/$app/AppDir" \
        "${staged[@]}" --version "$version" --repo "$SRCDIR"
    # The name carries flipctl and the architecture: a folder holds AppImages from
    # anywhere, and a person looking at it should be able to tell which are for the
    # panel. What decides that for flipctl is still the manifest inside.
    echo "== packing $app-flipctl-aarch64.AppImage =="
    rm -f "$OUT/$app-flipctl-aarch64.AppImage"
    in_bundle "$OUTDIR/tools/$TOOL" --appimage-extract-and-run -n --comp zstd \
        --mksquashfs-opt -Xcompression-level --mksquashfs-opt 19 \
        --runtime-file "$OUTDIR/tools/runtime-aarch64" \
        "$OUTDIR/$app/AppDir" "$OUTDIR/$app-flipctl-aarch64.AppImage" 2>&1 | grep -v "^$" | sed 's/^/  /'
    (cd "$OUT" && sha256sum "$app-flipctl-aarch64.AppImage" > "$app-flipctl-aarch64.AppImage.sha256")
    echo "== $(du -h "$OUT/$app-flipctl-aarch64.AppImage" | cut -f1) $OUT/$app-flipctl-aarch64.AppImage =="
}

# The squashfs sits right after the runtime, so its offset is the runtime's size.
check_bundle() {
    local file=$1
    local offset listing
    if [ "$NATIVE" = 1 ] && ! command -v unsquashfs >/dev/null; then
        echo "build.sh --native --check: on Debian, apt-get install squashfs-tools" >&2
        exit 1
    fi
    offset=$(stat -c %s "$TOOLS/runtime-aarch64")
    listing=$(in_bundle unsquashfs -o "$offset" -d "" -ll "$OUTDIR/$(realpath --relative-to="$OUT" "$file")")
    printf '%s\n' "$listing"
    for want in AppRun app.toml; do
        printf '%s\n' "$listing" | grep -qE "[ /]$want\$" || { echo "no $want at the root" >&2; exit 1; }
    done
}

if [ "${1:-}" = --native ]; then
    NATIVE=1
    SRCDIR=$HERE
    OUTDIR=$OUT
    TOOL=appimagetool-$(uname -m)
    if [ "$(uname -m)" = aarch64 ]; then
        XTARGET_ARG=()
        RELDIR=release
    fi
    shift
fi

[ $# -ge 1 ] || { usage; exit 2; }
mkdir -p "$OUT"
if [ "$NATIVE" = 1 ]; then
    preflight_native
else
    docker_image flipctl-cross "$HERE/ci/cross.Dockerfile" "$HERE/ci"
    docker_image flipctl-bundle "$HERE/ci/bundle.Dockerfile" "$HERE/ci"
fi
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
