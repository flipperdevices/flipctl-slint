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
- **`WorkingDirectory`.** App discovery is relative to it, so the apps this unit
  offers are the ones under `/usr/share/flipctl/apps`, not a checkout's. Without it
  flipctl starts happily and reports `apps 0 found`.
- **`ExecStopPost`.** Stopping the unit does not stop what it started: `PAMName=login`
  puts these processes in a logind session scope rather than the service's own
  cgroup, so a restart otherwise leaves the previous flipctl alive, still holding the
  remote view's port, and the new one comes up with nothing behind Back or the app
  switcher. Killing sway covers everything it hosts, since it is the parent of every
  hosted app.

`Conflicts=cog-seat1.service` because the panel has one owner at a time, and the
`Before=` lines claim it before a display manager wakes up and starts looking at
cards. The panel and the render node are named by path, never by card number, for
the same reason the rest of the stack does it: numbers move between kernels.
