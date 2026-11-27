# Running flipctl on the panel

`flipctl.service` runs flipctl on the SPI panel as the panel's only owner: it takes
DRM master on the panel's card, reads the Flipper's buttons from evdev, and starts
its own headless sway for the apps it hosts. sway never touches a DRM device, so
there is no second unit and nothing to order against.

The file is installed by hand for now, at `/etc/systemd/system/flipctl.service`; it
moves into the image overlays once the shape settles.

It runs the installed binary, `/usr/bin/flipctl`, never a build tree. A deploy
installs over that path and restarts this unit, and puts a dev run's own mode, port
and peer in a drop-in at `flipctl.service.d/50-deploy.conf` rather than editing this
file; deleting the drop-in leaves the machine running what the image shipped.

Three details cost real time to find, so they are worth stating rather than leaving
in the file as bare directives:

- **No login session.** flipctl once ran under `PAMName=login` on seat1, for the
  seat's devices. Groups give it those (`video`, `render`, `input`), and DRM master
  goes to the first opener without logind. A session cost 1.3s at boot, since opening
  one waits for `user@1000.service`, which is ordered after the network. It also
  moved flipctl out of the service's cgroup, so stopping the unit left it running.
- **`user-runtime-dir@1000.service` and lingering.** flipctl keeps its socket, sway's
  directory and each app's in `/run/user/1000`, and links PipeWire's sockets from
  there. That unit makes the directory without waiting for the network, and the
  image lingers the user so the directory, the user's bus and PipeWire stay up with
  nobody logged in.
- **`WorkingDirectory`.** The browser view's assets are found relative to the
  installed layout; apps are not, they are read from `/home/user/Apps` whatever the
  working directory.

## What the image must ship

Apps are AppImages under `/home/user/Apps`, and running them needs more of the
machine than the panel does. Each of these is installed by `build_deploy.sh` on a dev
device so it can be tried before the image carries it; the image is where they belong.

- The `fuse3` package: the AppImage runtime mounts a bundle through its setuid
  `fusermount3`. Without it flipctl still runs bundles, unpacking each into `/tmp`
  per launch, and says so in the log (`no fusermount3, extracting instead`).
- `70-flipctl-devices.rules` in `/etc/udev/rules.d/`: GPIO and SPI nodes to the
  `gpio` group and the network LEDs' attributes to `leds`. No USB devices: an app
  gets one through a claim.
- `flipctl.sysusers.conf` as `/etc/sysusers.d/flipctl.conf`: the `gpio` and `leds`
  groups. The
  unit names it in `SupplementaryGroups`, and a unit naming a missing group does not
  start.
- `flipctl-devices.conf` in `/etc/modules-load.d/`: `usbserial`, `cdc-acm`, `spidev`,
  loaded at boot. They were for the unit's device filter, which is gone, so this can
  go once `build-flipctl.sh` stops installing it.
- The user lingering, `/var/lib/systemd/linger/user`.
- The unit's own `SupplementaryGroups`, `Environment` and `user-runtime-dir@1000`
  lines, as in this directory.

`Conflicts=cog-seat1.service` because the panel has one owner at a time, and the
`Before=` lines claim it before a display manager wakes up and starts looking at
cards. The panel and the render node are named by path, never by card number, for
the same reason the rest of the stack does it: numbers move between kernels.

## Recovery

The recovery image ships this same unit with `flipctl-recovery.conf` installed as
`flipctl.service.d/50-recovery.conf`. Recovery has no logind, no sudo and no `user`,
so the drop-in runs flipctl as root with no PAM session, and unbinds fbcon from the
panel before it starts. Without a PAM session flipctl stays inside the unit's device
filter, which lists no block devices, so the drop-in lifts it: recovery's menus mount,
create and delete profiles. Nothing else orders it after the display driver, so the
drop-in waits for the panel's card as a device unit, which `flipctl-recovery.rules`
(installed as `70-flipctl-recovery.rules`) creates by tagging the card for systemd.
The image installs neither recovery file. Recovery's build has no `wayland` feature,
so there is no sway and the Apps and App Manager rows are not shown.
