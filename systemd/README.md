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

Four details cost real time to find, so they are worth stating rather than leaving
in the file as bare directives:

- **`PAMName=login` with `XDG_SEAT=seat1`.** The panel and the Flipper's buttons are
  tagged `ID_SEAT=seat1` by `72-seat-cog.rules`, and logind hands a session the
  devices of its own seat. Without the seat, a session comes up on seat0, which is
  the desktop's, and finds no buttons at all.
- **`XDG_SESSION_CLASS=user`.** logind refuses device control to a session of class
  `background`, with "Session class doesn't support taking device control", and
  `PAMName=login` alone produces exactly that class.
- **`WorkingDirectory`.** The browser view's assets are found relative to the
  installed layout; apps are not, they are read from `/home/user/Apps` whatever the
  working directory.
- **`ExecStopPost`.** Stopping the unit does not stop what it started: `PAMName=login`
  puts these processes in a logind session scope rather than the service's own
  cgroup, so a restart otherwise leaves the previous flipctl alive, still holding the
  remote view's port, and the new one comes up with nothing behind Back or the app
  switcher. Killing sway covers everything it hosts, since it is the parent of every
  hosted app.

## What the image must ship

Apps are AppImages under `/home/user/Apps`, and running them needs more of the
machine than the panel does. Each of these is installed by `build_deploy.sh` on a dev
device so it can be tried before the image carries it; the image is where they belong.

- The `fuse3` package: the AppImage runtime mounts a bundle through its setuid
  `fusermount3`. Without it flipctl still runs bundles, unpacking each into `/tmp`
  per launch, and says so in the log (`no fusermount3, extracting instead`).
- `70-flipctl-devices.rules` in `/etc/udev/rules.d/`: GPIO and SPI nodes to the
  `gpio` group, USB devices to `plugdev`.
- `flipctl.sysusers.conf` as `/etc/sysusers.d/flipctl.conf`: the `gpio` group. The
  unit names it in `SupplementaryGroups`, and a unit naming a missing group does not
  start.
- `flipctl-devices.conf` in `/etc/modules-load.d/`: `usbserial`, `cdc-acm`, `spidev`,
  loaded at boot because `DeviceAllow=char-<name>` is resolved against `/proc/devices`
  when the unit starts and a class nobody has registered is silently dropped.
- The unit's own `SupplementaryGroups` and `DeviceAllow` lines, as in this directory.

`Conflicts=cog-seat1.service` because the panel has one owner at a time, and the
`Before=` lines claim it before a display manager wakes up and starts looking at
cards. The panel and the render node are named by path, never by card number, for
the same reason the rest of the stack does it: numbers move between kernels.
