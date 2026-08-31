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
# image boots. The apps under /usr/share/flipctl/apps are left alone, since a
# machine can have apps this checkout knows nothing about.
#
# Usage:
#   ./build_deploy.sh                 headless, leaving the panel to its owner
#   ./build_deploy.sh --panel         drive the real panel and its buttons
#   ./build_deploy.sh --no-run        build only, install nothing, restart nothing
#   ./build_deploy.sh --status        report what is running, change nothing
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
# flipctl's own output is not in `journalctl -u flipctl`. PAMName=login puts the
# process in a logind session scope rather than the service's cgroup, and journald
# files a line under the cgroup that wrote it, so the unit view holds systemd's
# side of the story and nothing the program said. Ask by program name for that.
APP_LOG="sudo journalctl _COMM=flipctl --no-pager -o cat -n"

MODE=headless
RUN=yes
for arg in "$@"; do
    case "$arg" in
        --panel)   MODE=panel ;;
        --headless) MODE=headless ;;
        --no-run)  RUN=no ;;
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

if [ "$MODE" = status ]; then
    echo "== $HOST =="
    run "systemctl is-active $UNIT.service || true"
    run "grep -h '^ExecStart=.' $DROPIN 2>/dev/null || echo 'no deploy drop-in: the unit as installed'"
    run "sudo ss -ltnp 2>/dev/null | grep -E ':$PORT ' || echo 'nothing listening'"
    run "systemctl is-active cog-seat1.service || true" | sed 's/^/cog-seat1: /'
    run "systemctl is-active fake-flipctl-node-server.service || true" \
        | sed 's/^/prototype: /'
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

echo "== installing $BIN, $SHARE/assets and $SHARE/apps =="
# Into place by rename, never by writing over the file: the binary that is running
# is that same path, and writing to it fails with ETXTBSY. A rename replaces the
# directory entry, the running process keeps the inode it started with, and the
# restart below is what picks the new one up.
run "sudo install -m 755 ~/$DEST/target/release/flipctl $BIN.new && \
     sudo mv -f $BIN.new $BIN && \
     sudo mkdir -p $SHARE/assets/remote && \
     sudo cp -a ~/$DEST/crates/flipper-ui/assets/remote/. $SHARE/assets/remote/"

# The apps this checkout carries, sources only: build output and virtualenvs are
# per-machine and stale by the time they arrive. Copied in rather than synced, so
# apps a machine has of its own are left where they are.
#
# Owned by the user flipctl runs as, because an app is built where it sits: a Rust
# app compiles into its own target/ and a Python one gets a .venv beside its
# app.py, and neither can happen in a root-owned directory.
run "cd ~/$DEST && \
     sudo tar cf - --exclude=target --exclude=.venv --exclude=__pycache__ apps \
       | sudo tar xf - -C $SHARE && \
     for a in apps/*/; do sudo chown -R $USER_:$USER_ '$SHARE'/\$a; done"

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
# The kernel log goes in here too, not only in systemd/flipctl.service: a machine that came
# with its own unit keeps it, so the deploy carries what flipctl needs rather than assuming
# the shipped unit grants it. DeviceAllow is a list that adds to whatever the unit already
# says, so this is safe on a unit that has it. flipctl writes the node through sudo, but the
# cgroup's device filter applies to that child too, root or not, so without this line the
# panel's own timing never reaches the boot log.
run "sudo mkdir -p $(dirname "$DROPIN") && sudo tee $DROPIN >/dev/null" <<EOF
[Service]
ExecStart=
ExecStart=$BIN $ARGS
DeviceAllow=/dev/kmsg rw
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
