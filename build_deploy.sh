#!/bin/bash
# Copy this workspace to a Flipper One, build it there, install it, and restart
# flipctl.
#
# The host toolchain generally has no aarch64 std, so the build happens on the
# device: 8 cores and 8 GB make a cold build about 8 minutes and an incremental
# one about 25 seconds.
#
# The build tree is where the build happens and nothing else: what runs is the
# installed /usr/bin/flipctl, restarted through flipctl.service, the same one the
# image boots. Apps are AppImages in ~/Apps on the device, built by
# tools/appimage/build.sh and pushed with --apps; a deploy without it leaves them alone.
#
# Usage:
#   ./build_deploy.sh                 headless, leaving the panel to its owner
#   ./build_deploy.sh --panel         drive the real panel and its buttons
#   ./build_deploy.sh --cross         build here for aarch64 in docker, not on the device
#   ./build_deploy.sh --apps          also push target/appimage/*.AppImage to ~/Apps
#                                     (on its own: push them and build nothing)
#   ./build_deploy.sh --no-run        build only, install nothing, restart nothing
#   ./build_deploy.sh --status        report what is running, change nothing
#
# --cross is for a cold build, which is what a toolchain change, a freshly flashed
# device or a first checkout all are: measured on a 14-core host against this board,
# 3m03s here versus 13 minutes and counting there. A warm build is the other way
# round -- the device does an incremental in about 25s against 55s in the container,
# which pays for a bind-mounted target directory -- so the loop stays on the device
# and this is for the builds that hurt.
#
# Environment:
#   FLIPPER_HOST  default 192.168.1.110
#   FLIPPER_USER  default user
#   FLIPPER_PASS  default user; unset it to use key auth instead
#   REMOTE_PORT   default 8899
#   PEER          empty by default, since flipctl now serves 8899 itself and the
#                 prototype that used to hold it is gone; set host:port to compare
set -euo pipefail

HOST="${FLIPPER_HOST:-192.168.1.110}"
USER_="${FLIPPER_USER:-user}"
PASS="${FLIPPER_PASS-user}"
PORT="${REMOTE_PORT:-8899}"
# No default peer any more: it pointed at 127.0.0.1:8899, which is the port flipctl
# now serves, so the comparison would have been against ourselves.
PEER="${PEER-}"
DEST="flipctl-slint"
UNIT="flipctl"
BIN="/usr/bin/flipctl"
SHARE="/usr/share/flipctl"
# A deploy's arguments go in a drop-in, not in the unit: the unit on disk is the
# image's, and a mode, a port or a peer that only a dev run wants has no business
# rewriting it. Written fresh on every deploy, so no stale one survives, and
# removing it leaves the machine running exactly what the image shipped.
DROPIN="/etc/systemd/system/flipctl.service.d/50-deploy.conf"
# What running a bundle needs from the machine beyond the unit, each installed from
# systemd/ here until the image ships it. Listed once so --status can ask after them.
DEVICE_FILES="/etc/udev/rules.d/70-flipctl-devices.rules /etc/sysusers.d/flipctl.conf /etc/modules-load.d/flipctl-devices.conf"
# flipctl's own output is not in `journalctl -u flipctl`. PAMName=login puts the
# process in a logind session scope rather than the service's cgroup, and journald
# files a line under the cgroup that wrote it, so the unit view holds systemd's
# side of the story and nothing the program said. Ask by program name for that.
APP_LOG="sudo journalctl _COMM=flipctl --no-pager -o cat -n"

MODE=headless
RUN=yes
CROSS=no
APPS=no
# Whether anything was asked for beyond the bundles. `--apps` on its own means the
# bundles and nothing else: no source copy, no build, no restart. Without this a bare
# --apps falls into the device build below, which is minutes of compiling for a file
# copy nobody asked to compile for.
BUILD=no
for arg in "$@"; do
    case "$arg" in
        --panel)   MODE=panel; BUILD=yes ;;
        --headless) MODE=headless; BUILD=yes ;;
        --cross)   CROSS=yes; BUILD=yes ;;
        --apps)    APPS=yes ;;
        --no-run)  RUN=no; BUILD=yes ;;
        --status)  MODE=status ;;
        -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

# Password auth only if sshpass is present and FLIPPER_PASS is set; otherwise
# plain ssh, which picks up keys.
# One connection, reused. A deploy makes a dozen ssh calls and each was paying a
# full TCP and auth handshake; multiplexing them onto a single master cuts that to
# one. The socket lives in a temp dir and is closed on exit.
MUX_DIR=$(mktemp -d)
trap 'ssh -O exit -o ControlPath="$MUX_DIR/s" "$USER_@$HOST" 2>/dev/null; rm -rf "$MUX_DIR"' EXIT
MUX=(-o ControlMaster=auto -o ControlPath="$MUX_DIR/s" -o ControlPersist=60)

# Host keys are never stored: the device is reflashed often and every flash brings new
# ones, so a remembered key turns the next deploy into "REMOTE HOST IDENTIFICATION HAS
# CHANGED" and a manual ssh-keygen -R before anything works again.
KEYS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)
if [ -n "${PASS:-}" ] && command -v sshpass >/dev/null; then
    SSH=(sshpass -p "$PASS" ssh "${KEYS[@]}" "${MUX[@]}")
else
    SSH=(ssh "${KEYS[@]}" "${MUX[@]}")
fi
run() { "${SSH[@]}" "$USER_@$HOST" "$@"; }

# Refuse anywhere that is not a Flipper One. The demo takes DRM master on card0
# and stops cog, so a wrong host would fight a desktop compositor.
guard='test -e /sys/firmware/devicetree/base/model &&
       tr -d "\0" < /sys/firmware/devicetree/base/model | grep -qi flipper'
if ! run "$guard"; then
    echo "refusing: $HOST does not report itself as a Flipper One" >&2
    exit 1
fi

# The bundles. Built by tools/appimage/build.sh, not here: a deploy is a binary and a
# restart, and a bundle is a release artefact with a build of its own.
push_apps() {
    local here found app name
    here=$(cd "$(dirname "$0")" && pwd)
    found=no
    # Both kinds of app, because ~/Apps holds both: the bundles just built, and the
    # script apps in apps/, which are the file itself and need no build at all.
    for app in "$here"/target/appimage/*.AppImage "$here"/apps/*.py; do
        [ -e "$app" ] || continue
        found=yes
        name=$(basename "$app")
        echo "== pushing $name to ~/Apps =="
        run "mkdir -p ~/Apps && cat > ~/Apps/$name.new && chmod 755 ~/Apps/$name.new && mv -f ~/Apps/$name.new ~/Apps/$name" \
            < "$app"
    done
    if [ "$found" = no ]; then
        echo "--apps: nothing under target/appimage; run tools/appimage/build.sh first" >&2
    fi
}

# Bundles alone: push them and stop. flipctl is not rebuilt, reinstalled or
# restarted, because none of that is needed to put a file in a folder.
if [ "$APPS" = yes ] && [ "$BUILD" = no ]; then
    push_apps
    exit 0
fi

if [ "$MODE" = status ]; then
    echo "== $HOST =="
    run "systemctl is-active $UNIT.service || true"
    run "grep -h '^ExecStart=.' $DROPIN 2>/dev/null || echo 'no deploy drop-in: the unit as installed'"
    run "sudo ss -ltnp 2>/dev/null | grep -E ':$PORT ' || echo 'nothing listening'"
    run "systemctl is-active cog-seat1.service || true" | sed 's/^/cog-seat1: /'
    run "systemctl is-active fake-flipctl-node-server.service || true" \
        | sed 's/^/prototype: /'
    # What the image has to ship for bundles (systemd/README.md), and whether it does.
    run "dpkg-query -W -f='fuse3: \${db:Status-Status}\n' fuse3 2>/dev/null || echo 'fuse3: not installed'"
    run "for f in $DEVICE_FILES; do [ -e \"\$f\" ] && echo \"\$f: present\" || echo \"\$f: absent\"; done"
    run "systemctl show $UNIT -p SupplementaryGroups -p DeviceAllow"
    run "ls ~/Apps 2>/dev/null | sed 's/^/app: /' || true"
    run "$APP_LOG 12 2>/dev/null || true"
    exit 0
fi

echo "== copying source to $USER_@$HOST:~/$DEST =="
# tar over ssh rather than rsync: the device image has no rsync. target/ is 16 GB
# of build output and never travels.
tar czf - \
    --exclude target --exclude .git --exclude '*.actual.png' \
    -C "$(cd "$(dirname "$0")" && pwd)" . \
  | run "mkdir -p ~/$DEST && tar xzf - -C ~/$DEST"

# Where the binary to install ends up, which is what --cross changes.
BUILT="~/$DEST/target/release/flipctl"

if [ "$CROSS" = yes ]; then
    command -v docker >/dev/null || { echo "--cross needs docker" >&2; exit 2; }
    echo "== cross-building here for aarch64 =="
    # Its own target directory: the container's rustc and the host's are different
    # toolchains even at the same version, and sharing one directory makes each
    # rebuild what the other left, forever.
    HERE=$(cd "$(dirname "$0")" && pwd)
    XT="$HERE/target/cross"
    mkdir -p "$XT/home"
    # Which command builds the image: BuildKit is what docker means by a builder from
    # 23 on, and the CLI reaches it through the buildx plugin. Without that plugin
    # `docker build` falls back to the classic builder and says so on every run, and
    # DOCKER_BUILDKIT=1 fails outright rather than helping. Installing docker-buildx
    # retires the fallback; until then the fallback still builds this image.
    if docker buildx version >/dev/null 2>&1; then
        image=(docker buildx build --load)
    else
        image=(docker build)
    fi
    # Held rather than streamed: on a good build the classic builder's deprecation
    # notice is the only thing said, and a bad one has to be read in full.
    if ! said=$("${image[@]}" -q -t flipctl-cross \
                    -f "$HERE/ci/cross.Dockerfile" "$HERE/ci" 2>&1 >/dev/null); then
        printf '%s\n' "$said" >&2
        exit 1
    fi
    started=$SECONDS
    # As the invoking user, so nothing in the tree comes back owned by root. The
    # profile overrides are the device build's, so the two produce the same binary.
    docker run --rm -u "$(id -u):$(id -g)" \
        -v "$HERE:/src" -w /src \
        -v "$XT:/target" -e CARGO_TARGET_DIR=/target \
        -v "$XT/home:/cargo" -e CARGO_HOME=/cargo \
        -e CARGO_PROFILE_RELEASE_LTO=false -e CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
        flipctl-cross \
        cargo build --release --target aarch64-unknown-linux-gnu \
            -p flipctl --features device,slint,remote,wayland,gpu
    echo "== cross-built in $((SECONDS - started))s =="
    # Pushed as a file rather than built there, so the install below is the same
    # rename either way.
    BUILT="~/$DEST/flipctl.cross"
    run "mkdir -p ~/$DEST" </dev/null
    run "cat > ~/$DEST/flipctl.cross && chmod 755 ~/$DEST/flipctl.cross" \
        < "$XT/aarch64-unknown-linux-gnu/release/flipctl"
else

echo "== building on the device =="
# LTO off and 16 codegen units are for iteration speed. Drop both when measuring
# binary size.
#
# Cargo's own output is streamed rather than collected at the end. A cold build on
# the device takes minutes, and minutes of silence over ssh is indistinguishable
# from a connection that has died, so the lines that show movement come through as
# they happen: each crate as it starts, numbered, so the count is the progress, and
# every error with the source lines under it. The build's exit status is taken from
# the pipe, not from the filter, which would otherwise report success for a build
# that failed without printing a line the filter keeps.
started=$SECONDS
set +e
run "cd ~/$DEST && export PATH=\$HOME/.cargo/bin:\$PATH \
        CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 && \
     cargo build --release -p flipctl --features device,slint,remote,wayland,gpu 2>&1" \
  | awk '
        { sub(/^[ \t]+/, "") }
        /^(Compiling|Building|Fresh|Finished|Downloaded|Updating) / {
            printf "  [%3d] %s\n", ++n, $0
            fflush()
            next
        }
        /^(error|warning)/ || /^(-->|\||=) / { print; fflush() }'
status=${PIPESTATUS[0]}
set -e
if [ "$status" -ne 0 ]; then
    echo "== build failed after $((SECONDS - started))s ==" >&2
    exit "$status"
fi
echo "== built in $((SECONDS - started))s =="
fi

if [ "$RUN" = no ]; then
    echo "== built, nothing installed and nothing restarted =="
    exit 0
fi

ARGS="--remote 0.0.0.0:$PORT --assets $SHARE/assets/remote"
[ -n "$PEER" ] && ARGS="$ARGS --peer $PEER"
if [ "$MODE" = panel ]; then
    # The panel is single-owner. cog holds card0 and has to let go first, and the
    # prototype draws on that same glass through cog, so it goes too: headless mode
    # is where the two share, and this mode is where we take it.
    echo "== releasing the panel from cog and the prototype =="
    # Only what is actually up. A machine that never had the prototype installed
    # otherwise answers "Unit fake-flipctl-node-server.service not loaded", which
    # reads as a failure when it is the normal state.
    run 'for u in cog-seat1 fake-flipctl-node-server; do
             if [ "$(systemctl is-active $u.service 2>/dev/null)" = active ]; then
                 echo "stopping $u"
                 sudo systemctl stop $u.service || true
             fi
         done'
    ARGS="--panel $ARGS"
else
    # Headless leaves the panel to whoever has it, so the prototype can keep
    # rendering on glass while this serves the browser.
    ARGS="--headless $ARGS"
fi

echo "== installing $BIN and $SHARE/assets =="
# Into place by rename, never by writing over the file: the binary that is running
# is that same path, and writing to it fails with ETXTBSY. A rename replaces the
# directory entry, the running process keeps the inode it started with, and the
# restart below is what picks the new one up.
run "sudo install -m 755 $BUILT $BIN.new && \
     sudo mv -f $BIN.new $BIN && \
     sudo mkdir -p $SHARE/assets/remote && \
     sudo cp -a ~/$DEST/crates/flipper-ui/assets/remote/. $SHARE/assets/remote/"

if [ "$APPS" = yes ]; then
    push_apps
fi

# What a bundle needs from the machine, until the image ships it (systemd/README.md
# lists the same things as the image's obligation). The AppImage runtime mounts a
# bundle through fuse3's setuid fusermount3; without it flipctl unpacks each launch
# into /tmp and says so in its log, so this is a slowness, not a failure, and the
# message here is what keeps the image requirement from being forgotten.
if ! run "dpkg-query -W -f='\${db:Status-Status}' fuse3 2>/dev/null | grep -qx installed"; then
    echo "== installing fuse3: an image requirement this image does not meet yet ==" >&2
    run "sudo DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
         sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends fuse3"
fi
# In this order: the group before the unit that names it restarts, since a unit
# naming a missing group does not start; the drivers before the unit resolves its
# device classes against /proc/devices.
echo "== installing the device rules, group and modules =="
SYSD="$(dirname "$0")/systemd"
run "sudo mkdir -p /etc/sysusers.d /etc/udev/rules.d /etc/modules-load.d" </dev/null
run "sudo tee /etc/sysusers.d/flipctl.conf >/dev/null && sudo systemd-sysusers /etc/sysusers.d/flipctl.conf" \
    < "$SYSD/flipctl.sysusers.conf"
run "sudo tee /etc/udev/rules.d/70-flipctl-devices.rules >/dev/null && \
     sudo udevadm control --reload && \
     sudo udevadm trigger -s gpio -s usb -s spidev -c change" \
    < "$SYSD/70-flipctl-devices.rules"
run "sudo tee /etc/modules-load.d/flipctl-devices.conf >/dev/null && \
     sudo systemctl restart systemd-modules-load.service" \
    < "$SYSD/flipctl-devices.conf"

# The unit itself, where the machine has none. Stock profiles do not ship it -- it has
# always been installed by hand -- so a deploy onto a freshly installed profile otherwise
# builds, installs the binary, and then has nothing to restart. Never overwritten: a
# machine whose unit someone has tuned keeps it, and only the drop-in below is ours.
if ! run "systemctl cat $UNIT.service >/dev/null 2>&1"; then
    echo "== installing $UNIT.service, which this profile does not have =="
    run "sudo tee /etc/systemd/system/$UNIT.service >/dev/null" < "$(dirname "$0")/systemd/$UNIT.service"
    run "sudo systemctl enable $UNIT.service" 2>&1 | tail -1
fi

echo "== restarting $UNIT.service ($MODE) =="
# The unit's device and group lines go in here too, not only in systemd/flipctl.service:
# a machine that came with its own unit keeps it, so the deploy carries what flipctl
# needs rather than assuming the shipped unit grants it. Both settings are lists that
# add to whatever the unit already says, so this is safe on a unit that has them, and
# they are read from the unit here rather than written twice.
run "sudo mkdir -p $(dirname "$DROPIN") && sudo tee $DROPIN >/dev/null" <<EOF
[Service]
ExecStart=
ExecStart=$BIN $ARGS
$(grep -E '^(DeviceAllow|SupplementaryGroups)=' "$SYSD/$UNIT.service")
EOF
# Not fatal here: a unit that fails to come up is reported below, with its log,
# which is more use than the shell aborting on the restart's exit status.
run "sudo systemctl daemon-reload && sudo systemctl restart $UNIT.service" || true

# Wait for it rather than sleeping a guessed amount.
for _ in $(seq 30); do
    state=$(run "systemctl is-active $UNIT.service" || true)
    [ "$state" = active ] && break
    sleep 1
done
if [ "$state" != active ]; then
    echo "== failed to start ==" >&2
    # Both halves: what systemd made of the unit, then what the program said before
    # it went away.
    run "sudo journalctl -u $UNIT -n 10 --no-pager -o cat" >&2
    run "$APP_LOG 20" >&2
    exit 1
fi

echo "== running =="
run "$APP_LOG 6"
echo
echo "  http://$HOST:$PORT/          the panel in a device photo"
echo "  http://$HOST:$PORT/diff      side-by-side comparison and controls"
