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
    # dependency whose license is not in about.toml's accepted list.
    tmp=$(mktemp)
    cargo about generate --fail \
        --manifest-path bin/flipctl/Cargo.toml \
        --features device,slint,remote,wayland,gpu \
        -c about.toml about.hbs -o "$tmp" >/dev/null 2>&1
    if cmp -s "$tmp" THIRD-PARTY-LICENSES.md; then
        echo "THIRD-PARTY-LICENSES.md: up to date"
    else
        echo "THIRD-PARTY-LICENSES.md is stale: run scripts/gen-third-party-licenses.sh" >&2
        rm -f "$tmp"
        exit 1
    fi
    rm -f "$tmp"
else
    echo "third-party licenses: skipped, no cargo-about"
fi

echo "== all green =="
