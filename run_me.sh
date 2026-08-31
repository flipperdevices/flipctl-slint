#!/bin/sh
# Install what is built here and restart flipctl, taking the panel back from the
# prototype.
#
# Meant to be run ON the Flipper One, from the tree build_deploy.sh copies over:
#
#     ssh user@flipper 'cd flipctl-slint && ./run_me.sh'
#
# It builds nothing. Use build_deploy.sh to copy and compile; this is the last two
# steps of a deploy, which are the ones worth having as one command on the device:
# the build tree is where a build happens, and /usr/bin/flipctl is what runs.
#
# Three units share the glass:
#
#   cog-seat1                 the prototype's browser, holding DRM master
#   fake-flipctl-node-server  the prototype's own server, if a machine still has
#                             one: it wants 8899, which flipctl now serves itself
#   flipctl                   this, from /usr/bin, restarted below
#
# The arguments are the unit's own, so a deploy drop-in left by build_deploy.sh
# --headless still applies here. Remove
# /etc/systemd/system/flipctl.service.d/50-deploy.conf to get back to exactly what
# the image ships.
set -eu

DEST="$(cd "$(dirname "$0")" && pwd)"
BUILT="$DEST/target/release/flipctl"
BIN=/usr/bin/flipctl
SHARE=/usr/share/flipctl

# This takes DRM master and stops a compositor, so it refuses anywhere that is
# not the device. Same guard build_deploy.sh applies from the other end.
model=/sys/firmware/devicetree/base/model
if ! [ -e "$model" ] || ! tr -d '\0' < "$model" | grep -qi flipper; then
    echo "refusing: this is not a Flipper One" >&2
    exit 1
fi
if ! [ -x "$BUILT" ]; then
    echo "no binary at $BUILT; run build_deploy.sh first" >&2
    exit 1
fi

# The panel is single-owner: whoever holds card0 has to let go before we can.
for unit in cog-seat1 fake-flipctl-node-server; do
    if [ "$(systemctl is-active "$unit.service" 2>/dev/null)" = active ]; then
        echo "stopping $unit"
        sudo systemctl stop "$unit.service" || true
    fi
done

# Into place by rename, never by writing over the file: that path is the binary
# that is running, and writing to it fails with ETXTBSY. A rename replaces the
# directory entry, the running process keeps the inode it started with, and the
# restart is what picks the new one up.
echo "installing $BIN"
sudo install -m 755 "$BUILT" "$BIN.new"
sudo mv -f "$BIN.new" "$BIN"
sudo mkdir -p "$SHARE/assets/remote"
sudo cp -a "$DEST/crates/flipper-ui/assets/remote/." "$SHARE/assets/remote/"

# Sources only, and owned by the user flipctl runs as: an app is built where it
# sits, so a Rust app needs to write its target/ and a Python one its .venv. Apps
# a machine has of its own are left alone.
echo "installing $SHARE/apps"
sudo tar cf - -C "$DEST" --exclude=target --exclude=.venv --exclude=__pycache__ \
    apps | sudo tar xf - -C "$SHARE"
for a in "$DEST"/apps/*/; do
    sudo chown -R "$(id -un):$(id -gn)" "$SHARE/apps/$(basename "$a")"
done

echo "restarting flipctl"
sudo systemctl restart flipctl.service || true

i=0
while [ "$i" -lt 30 ]; do
    state=$(systemctl is-active flipctl.service 2>/dev/null || true)
    [ "$state" = active ] && break
    i=$((i + 1))
    sleep 1
done
if [ "${state:-}" != active ]; then
    echo "failed to start:" >&2
    sudo journalctl -u flipctl -n 10 --no-pager -o cat >&2
    sudo journalctl _COMM=flipctl -n 20 --no-pager -o cat >&2
    exit 1
fi

# By program name, not by unit: PAMName=login puts flipctl in a logind session
# scope, and journald files a line under the cgroup that wrote it, so the unit view
# has systemd's messages about the unit and nothing the program said.
sudo journalctl _COMM=flipctl -n 6 --no-pager -o cat
echo
PORT=$(systemctl show -p ExecStart --value flipctl.service \
       | tr ' ' '\n' | sed -n 's/^0\.0\.0\.0://p' | head -1)
echo "  http://$(hostname -I 2>/dev/null | awk '{print $1}'):${PORT:-8899}/"
