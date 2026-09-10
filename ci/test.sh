#!/bin/sh
# Everything CI runs. Use this rather than a bare `cargo test`.
#
# The rendering tests are gated on the `screens` feature, which is not a default, so
# a bare `cargo test` compiles none of them and reports success while they are
# broken. That happened: a change to the ListItem struct left three of them
# failing to compile and the suite still went green.
#
# The browser view's tests are gated the same way, on `remote`.
set -e

echo "== token and font tests (no renderer) =="
cargo test --quiet

echo "== rendering tests (needs the compiled components) =="
cargo test --quiet -p flipper-ui --features screens

echo "== the browser view (needs the remote feature) =="
cargo test --quiet -p flipper-ui --features remote

echo "== the terminal front end (needs the tui feature) =="
# --examples for the same reason the other probes are built: tui_probe is the only
# way the drawing gets looked at off the device, so it must not break silently.
cargo test --quiet -p flipper-ui --features tui --examples

echo "== app bundles and scripts (needs the bundle feature) =="
cargo test --quiet -p flipper-ui --features bundle --test app
cargo test --quiet -p flipper-ui --features bundle --lib script::

echo "== the app bundler =="
if command -v python3 >/dev/null 2>&1; then
    python3 -m unittest discover -q -s tools/appimage -p 'test_*.py'
else
    echo "bundler tests: skipped, no python3"
fi
if command -v shellcheck >/dev/null 2>&1; then
    shellcheck tools/appimage/build.sh tools/appimage/AppRun.in && echo "shellcheck: clean"
else
    echo "shellcheck: skipped"
fi

echo "== the panel binary (the only build that compiles main.rs) =="
# Every other pass here is a library test, and none of them compile the binary at
# all: a duplicate function in main.rs survived a rebase, a reformat and several
# green runs because nothing ever built it. panel() exists only under device+slint,
# and the deploy uses the full set, so that is what is built.
cargo build --quiet -p flipctl --features "slint device wayland gpu remote"

echo "== formatting =="
# rustfmt.toml is the argument about style; this is what keeps it true. A table that
# is deliberately wider than the limit carries #[rustfmt::skip].
cargo fmt --all --check

echo "== no raw colours or panel dimensions =="
./ci/no-raw-colours.sh

# Both of these need a tool that is not part of the build, so they say what is
# missing rather than failing the suite on a machine that has not installed it.
# CI installs both, so there they are checks and not notices.
echo "== licensing =="
if command -v reuse >/dev/null 2>&1; then
    reuse lint --quiet && echo "REUSE: compliant"
else
    echo "REUSE: skipped, no reuse (pip install reuse)"
fi
if cargo about --version >/dev/null 2>&1; then
    # Regenerate into a temporary file and compare: the point is that the
    # committed attribution still matches Cargo.lock, and --fail catches a
    # dependency whose license is not in about.toml's accepted list. The same for
    # each app, whose bundle carries its own file.
    check_licenses() {
        tmp=$(mktemp)
        cargo about generate --fail --manifest-path "$1" $2 \
            -c about.toml about.hbs -o "$tmp" >/dev/null 2>&1
        if cmp -s "$tmp" "$3"; then
            echo "$3: up to date"
        else
            echo "$3 is stale: run scripts/gen-third-party-licenses.sh" >&2
            rm -f "$tmp"
            exit 1
        fi
        rm -f "$tmp"
    }
    check_licenses bin/flipctl/Cargo.toml "--features device,slint,remote,wayland,gpu" THIRD-PARTY-LICENSES.md
    for app in apps/*/Cargo.toml; do
        check_licenses "$app" "" "$(dirname "$app")/THIRD-PARTY-LICENSES.md"
    done
else
    echo "third-party licenses: skipped, no cargo-about"
fi

echo "== all green =="
