#!/bin/bash
# Lay the built apps out as a folder flipctl can read, for whoever is installing them.
#
# Usage:
#   tools/appimage/stage.sh <dest>
#
# The bundles have to be built already; this only arranges them. What comes out is
# exactly what flipctl walks: an AppImage per app, under the folder its manifest asks
# for, the script apps at the path they sit at in apps/, and a folder's icon.png
# beside them. The image installs this into its read-only apps folder and a person
# copying to a device drops it into ~/Apps; both are the same tree, which is the point
# of doing it here rather than twice.
#
# What counts as an app is decided the way flipctl decides it, so nothing here has a
# list to keep up to date:
#
#   * an AppImage in target/appimage, named after the directory under apps/ it was
#     built from, which is where its manifest and its `folder` are read from
#   * any file whose head carries a `/// flipctl` block, whatever its extension, as
#     long as no directory above it holds an app.toml: those are a bundle's own
#     sources, not apps
#   * icon.png in a folder, which is that folder's own icon and has no block to be
#     found by
set -euo pipefail

HERE=$(cd "$(dirname "$0")/../.." && pwd)
BUILT="$HERE/target/appimage"

[ $# -eq 1 ] || { sed -n '2,5p' "$0"; exit 2; }
DEST=$1

# The folder an app asks to be filed under, from the source manifest: a bundle carries
# only the keys flipctl itself reads, and this is the installer's business.
folder_of() {
    local app=$1
    sed -n 's/^folder *= *"\(.*\)"/\1/p' "$HERE/apps/$app/app.toml" 2>/dev/null | head -n1
}

# Whether any directory at or above this one holds a manifest, which makes the file a
# bundle's source rather than an app in its own right.
inside_a_bundle() {
    local dir=$1
    while [ "$dir" != "." ] && [ "$dir" != "/" ]; do
        [ -e "$HERE/apps/$dir/app.toml" ] && return 0
        dir=$(dirname "$dir")
    done
    return 1
}

mkdir -p "$DEST"

staged=0
for image in "$BUILT"/*.AppImage; do
    [ -e "$image" ] || continue
    name=$(basename "$image")
    app=${name%-flipctl-aarch64.AppImage}
    folder=$(folder_of "$app")
    mkdir -p "$DEST${folder:+/$folder}"
    install -m 755 "$image" "$DEST${folder:+/$folder}/$name"
    echo "stage: $name${folder:+ in $folder}"
    staged=$((staged + 1))
done

# The scripts and the folder icons, at the path they already sit at.
while IFS= read -r rel; do
    dir=$(dirname "$rel")
    inside_a_bundle "$dir" && continue
    if [ "$(basename "$rel")" != icon.png ] && ! head -c 8192 "$HERE/apps/$rel" | grep -q "/// flipctl"; then
        continue
    fi
    mkdir -p "$DEST/$dir"
    install -m 644 "$HERE/apps/$rel" "$DEST/$rel"
    echo "stage: $rel"
    staged=$((staged + 1))
done < <(cd "$HERE/apps" && find . -type f -not -path "*/__pycache__/*" -not -path "*/target/*" \
    -printf '%P\n' | sort)

# A script app is only startable if something provides its runtime, and a staging with
# no bundles at all means build.sh was never run.
[ "$staged" -gt 0 ] || { echo "stage: nothing to stage; run tools/appimage/build.sh --all first" >&2; exit 1; }
echo "stage: $staged files in $DEST"
