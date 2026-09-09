# Writing an app

An app is an ordinary Wayland client. flipctl runs a headless compositor, gives the
app an output sized to what it draws and a workspace of its own, reads its frames
and puts them on the panel. Nothing describes the app's screen to flipctl: the app
owns every pixel of its surface, and what keeps it looking like the device is that
it draws with flipctl's own widgets.

## Where an app lives

An app is delivered as one file in the user's `Apps` folder, either an AppImage or a
script:

    /home/user/Apps/radio-flipctl-aarch64.AppImage       Internet radio, at the top of the list
    /home/user/Apps/stations.py                  a script, run through its runtime
    /home/user/Apps/Network/nmap-flipctl-aarch64.AppImage inside the Network folder

`/home` is the one subvolume every profile shares, so the folder survives a factory
reset and serves every profile. A folder is a group: the Apps list shows how many
apps are inside, Ok walks into it, Back comes back out, and the list opens at the top
each time. Nothing is installed to run an app; the file is the whole of it.

What makes a file ours is a manifest: `app.toml` at the root of a bundle's squashfs,
or a `# /// flipctl` block in a script's head. A file with neither is not listed, and
the log says so once (`bundles        Foo: not a flipctl app, skipped`). flipctl reads
the manifest and the icon out of a bundle without running it, keeps them under
`~/.cache/flipctl/bundles/<key>/`, and does not open an unchanged file again; a script
is read as text every scan, which costs one `read` of its first 8KB.

Neither kind is ever executed to find out what it is. The folder is where a person
drops anything, and the scan runs in a unit that can reach the GPIO and the USB bus.

## The manifest

`app.toml`, in the source directory under `apps/` and copied into the bundle:

| Field | What |
|---|---|
| `name` | What the Apps menu shows. |
| `wayland` | The command. In the source it is what the bundler turns into the bundle's entry point; in the bundle it is `./AppRun`, and flipctl only needs it non-empty. |
| `icon` | A PNG beside the manifest, drawn on the app's row: 14px wide, the alpha is the shape, and a strip of 14px frames animates while the row is selected, like the menu's own icons. Without one the bundler puts in its own. |
| `size` | `"320x200"` for a program that insists on drawing at its own size. The output is made that shape and the frame is scaled to the panel. |
| `apt` | Debian packages the app needs from the device. flipctl checks them with dpkg before the first launch and offers the install. |
| `audio` | Links the PipeWire sockets into the app's runtime directory and pins the panel's sink. |
| `status` | flipctl paints its own status strip over the app's frame, for a program that cannot draw one. An app on the framework draws the real bar itself and leaves this off. |
| `rotate` | `"left"` or `"right"` for a portrait app. |
| `env` | Extra environment, one `"KEY=value"` per entry. Applied last, so it overrides what the launch sets. |
| `runtime` | The runtime this app is run through, e.g. `"python"`. A bundle declaring the same word in `provides` is the launcher that runs it. A script defaults to the runtime its extension implies. An app whose runtime nothing provides is listed and refused with a sentence naming it. |
| `provides` | The runtime word a launcher bundle answers to, e.g. `"py"`. `apps/python-runtime` is the worked example. |

## A script is an app

A `.py` in the folder carries its manifest in its own head, in the block shape PEP 723
defines, beside PEP 723's own block naming what it needs installed:

    #!/usr/bin/env python3
    # /// script
    # dependencies = ["httpx"]
    # ///
    # /// flipctl
    # name = "Stations"
    # icon = "stations.png"
    # audio = true
    # ///
    import flipctl

Every manifest field means what it means for a bundle. Two are filled in when the
block leaves them out: `runtime`, from the extension, and `wayland`, from the file's
own name, because a script names no command of its own. An icon is a PNG beside it.

`apps/ping.py` is the worked example, and it is the whole app: a file in the tree
beside the bundle directories, pushed to `~/Apps` as it stands by `--apps`, with
nothing to build. It draws with the panel's own widgets through the `flipctl` package
its runtime carries.

What actually starts it is the launcher bundle whose `provides` matches its `runtime`,
with the script as its argument, in a working directory of the script's own under
`~/.local/share/flipctl/apps/<key>/`. With no such launcher installed the script is
still listed, and starting it says which runtime is missing.

## How a bundle runs

flipctl runs the file itself, from a writable directory of the app's own under
`~/.local/share/flipctl/apps/<key>/`, with `FLIPCTL_HOSTED=1` in the environment
alongside the Wayland display and the rest. The AppImage runtime mounts the squashfs
over FUSE and runs `AppRun`, which puts `usr/bin` and `usr/lib/aarch64-linux-gnu`
from the bundle ahead of the device's own and execs the program. A device without
`fuse3` still works: flipctl notices there is no `fusermount3`, sets
`APPIMAGE_EXTRACT_AND_RUN=1`, and the runtime unpacks the image into `/tmp` per launch
instead, slower and said in the log.

Started anywhere else, `AppRun` finds `FLIPCTL_HOSTED` unset and hands the file over:

    flipctl open /home/user/Apps/radio-flipctl-aarch64.AppImage

connects to the running flipctl's socket at `$XDG_RUNTIME_DIR/flipctl.sock`, one line
each way, and flipctl lists the bundle if it is new, starts it, or brings it to the
front if it is already on the panel. So a double-click in the desktop's file manager
puts the app on the panel rather than on the HDMI screen. The exit status says what
happened: 0 on the panel, 1 refused (`not a flipctl app`, `cannot host apps`), 2
nothing listening.

A hosted app runs with the unit's groups and device classes: GPIO, SPI, I2C, USB and
USB serial are open to it (`systemd/README.md` lists what the image ships for that).

## Bundling

    tools/appimage/build.sh apps/radio        target/appimage/radio-flipctl-aarch64.AppImage
    tools/appimage/build.sh --all
    tools/appimage/build.sh --check target/appimage/radio-flipctl-aarch64.AppImage
    ./build_deploy.sh --cross --panel --apps  push what was built to ~/Apps

The build runs on the x86_64 host and nothing aarch64 executes there: the program is
cross-built in the `flipctl-cross` container, `tools/appimage/bundle.py` lays out the
AppDir in the `flipctl-bundle` container, and appimagetool packs it with zstd behind
the aarch64 runtime, both pinned by sha256 in `tools/appimage/tools.lock`. The
program's link surface is checked on the way: a framework app needs `libc`, `libm`
and `libgcc_s` and nothing else, and anything more fails the build by name.
`SOURCE_DATE_EPOCH` is the commit's time, so two builds of one tree give one hash.

Only a Rust app on the framework is bundled today. Its `apt` packages stay the
device's: the radio's `mpv` is in the Desktop image, and bundling Debian's mpv would
drag ffmpeg's whole dependency fan-out in (measured: 118 packages, 177 MB extracted)
for a 5 MB app. Bundling a package closure, a Python interpreter or a program out of
the archive are extensions of the same tooling, not written yet.

## The framework

`crates/flipctl-app` is what an app draws with. A Rust app takes it as a dependency
and as a build dependency:

    [dependencies]
    flipctl-app = { path = "../../crates/flipctl-app" }
    slint = { version = "1.17", default-features = false, features = [
        "compat-1-2", "backend-winit-wayland", "renderer-software", "std" ] }

    [build-dependencies]
    flipctl-app = { path = "../../crates/flipctl-app", features = ["build"] }

`build.rs` is one line, and it puts the widget library under `@flipctl` and the
generated theme under `@theme`:

    fn main() {
        flipctl_app::build::compile("ui/app.slint");
    }

What the crate gives the app:

| Item | What |
|---|---|
| `Shell` | The 256x144 window, the panel's fonts, and the keyboard wired up. Inherit it and forward its `key` callback to one of your own, which is what Rust can then bind. |
| `PanelStatus` | A Slint global the status bar reads. Set it once and every bar in the app shows it. |
| `Key::from_slint` | The panel's buttons by name. They arrive as ordinary keys: the D-pad as arrows, Ok as Return, Back as Backspace, and the five soft buttons as z x c v b. |
| `Key::soft_slot` | Which soft key was pressed, for the pressed state on the bar. |
| `apply_status!` | Fills `PanelStatus` from a `StatusSource`, which reads sysfs. An app has the same access flipctl does. |
| `picture` | A `paint::Surface` as an image, for an app that draws pixels. |
| `paint`, `font`, `pixel`, `theme` | The primitives, the panel's bitmap fonts, and every token. |
| `dropdown` | The settings row's own geometry: what to put in a `DropRow`, and where a picker's options go. |

## The widgets

Imported from `@flipctl`, the same components flipctl's own screens are built from:

| Component | For |
|---|---|
| `MenuBody` (`list.slint`) | A list of rows with a selector, a scrollbar and the soft bar. |
| `DetailBody` (`detail.slint`) | Rows that can be a pair, a gauge or a rule. Set `fit_gauges` so the bars are sized around your labels. |
| `CardsBody` (`card.slint`) | Framed cards, each worth two lines, stacked down the page. |
| `LogBody` (`log.slint`) | Lines of output with a scrollbar. |
| `CanvasBody` (`canvas.slint`) | Your own picture, with text placed and measured on this side, and the soft bar over it. |
| `TextInputBody` (`keyboard.slint`) | The on-screen keyboard. |
| `DropLine`, `DropPicker` (`dropdown.slint`) | A settings row with a value chip or a slider, and the picker that opens over it. Build the rows with `dropdown`. |
| `Modal` (`modal.slint`) | A dialog over whatever is behind it. |
| `SoftBar`, `SoftButton` (`frame.slint`) | The five soft keys on their own. |
| `StatusBar` (`statusbar.slint`) | The top row, if you are not using a body that draws it. |

## The two examples

`apps/sysmon` is the fuller one: four pages, three of rows in `DetailBody` and one
that paints two graphs into a `Surface` and shows them through `CanvasBody`, with
the soft keys switching pages. It needs nothing from the device but `/proc` and `/sys`.

`apps/radio` is the one that runs something: four `DropLine` rows over a child mpv,
which it starts, asks questions of over a socket and kills with itself. It is also the
one with tests that draw: `cargo test` renders the page headlessly and reads the
pixels back, and `RADIO_RENDER=1 cargo test` leaves the frames in `target/render` to
look at.

## What it costs

Slint links statically, so an app's binary is around 13MB stripped, about 5MB in the
bundle's zstd squashfs, plus the runtime's 0.9MB. A cold cross build of an app takes a
few minutes on a host; a warm one, seconds.

## Licensing

An app that draws with the framework links `flipctl-app`, and through it Slint, so
the app's binary is GPL-3.0-only while its own source stays MIT. That is the same
combination flipctl itself ships. Each app's `THIRD-PARTY-LICENSES.md`, generated by
`scripts/gen-third-party-licenses.sh` beside flipctl's own, travels in the bundle
under `usr/share/doc/<program>/`, with our license, the texts in `LICENSES/` and the
three fonts' terms, since their glyphs are compiled into the binary.
