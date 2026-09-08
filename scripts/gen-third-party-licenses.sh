#!/bin/sh
# Regenerate THIRD-PARTY-LICENSES.md, the license attribution for every crate
# linked into the shipped binary, from the current Cargo.lock, and the same file for
# each app under apps/, whose bundle carries it.
#
# Run this whenever dependencies change. Requires `cargo-about`:
#     cargo install cargo-about --features cli
#
# Config lives in about.toml (accepted licenses, target, dev-dep exclusion) and
# the output layout in about.hbs.
#
# Generated for each binary as it ships, not for the workspace: the panel build
# enables the DRM sink, the browser view, hosting and the GPU converter, each of
# which pulls crates of its own, while an app links the framework's Wayland backend
# that the panel binary never does. One file per binary is the only shape that is
# right for both.
set -eu

cd "$(dirname "$0")/.."

if ! cargo about --version >/dev/null 2>&1; then
    echo "error: cargo-about not found, install with:" >&2
    echo "    cargo install cargo-about --features cli" >&2
    exit 1
fi

# --fail: error out if any linked crate uses a license not in about.toml's
# `accepted` list, so a new dependency can never silently ship unlicensed.
cargo about generate --fail \
    --manifest-path bin/flipctl/Cargo.toml \
    --features device,slint,remote,wayland,gpu \
    -c about.toml about.hbs -o THIRD-PARTY-LICENSES.md
echo "wrote THIRD-PARTY-LICENSES.md"
for app in apps/*/Cargo.toml; do
    out="$(dirname "$app")/THIRD-PARTY-LICENSES.md"
    cargo about generate --fail --manifest-path "$app" -c about.toml about.hbs -o "$out"
    echo "wrote $out"
done
