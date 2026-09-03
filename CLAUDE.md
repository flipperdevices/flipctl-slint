# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`README.md` has the layout table, the licensing rules and the deploy flags. This
file is what is not written down anywhere else: the invariants that fail *silently*,
and the division of labour that decides which side of the Rust/Slint line new code
goes on.

## Commands

    sh ci/test.sh                    everything CI runs. Use this, not bare cargo test.
    ./ci/no-raw-colours.sh           refuses a colour or panel dimension outside tokens.toml

**`cargo test` alone is a trap.** The rendering tests are gated on the `screens`
feature and the browser view's on `remote`, so a bare run compiles neither and
reports success while they are broken. That has happened. `ci/test.sh` runs all
three passes.

    cargo test -p flipper-ui --lib wifi                          unit tests, by name
    cargo test -p flipper-ui --features screens --test wifi      one golden test file
    cargo test -p flipper-ui --features remote --test remote      the browser view
    FLIPPER_UI_BLESS=1 cargo test -p flipper-ui --features screens    rewrite goldens

A blessed golden is not a passing test until somebody has looked at it. They are
8-bit greyscale PNGs at panel resolution, so open them (scaled 3x with nearest
neighbour) and read the pixels before committing.

    cargo build -p flipctl --features "slint device"      the real panel() exists only here
    cargo build -p flipctl --features "slint device remote wayland gpu"

Feature-gate care: `panel()` is `#[cfg(all(feature = "device", feature = "slint"))]`
and there is a stub for every other combination. A `--features slint` build
therefore compiles the stub, succeeds, and dead-code-warns half the file. Always
build with `device,slint` when touching the render loop.

    cargo run -p flipctl --features slint -- --png /tmp/x.png --screen network --select 2

One frame, headless, no device. `--screen` takes `main`, `network`, `settings`,
`ethernet`, `routing`, `disk`, `battery`, `modem`, `update`.

## Building for the device

Never on the local host: it has no aarch64 std. The build happens **on the
Flipper**, over ssh, by `./build_deploy.sh --panel` (see README).

A device needs a toolchain of its own, and trixie's own rustc 1.85 is below the
1.92 slint 1.17 requires. Debian's backported 1.94 does the job -- verified by
building `apps/sysmon` with nothing but `cargo rustc libstd-rust-dev
libstd-rust-1.94 libllvm21` installed and rustup deleted -- and those packages are
in the flipper archive, so `apt-get install cargo` is enough. `libllvm21` is the
one to remember: trixie main has no LLVM 21 and rustc needs it. rustup works too
and takes precedence when present, since `app::cargo()` looks in `$HOME/.cargo/bin`
first.

## Apps are built where they sit, including when installed

`docs/apps.md` says an app is built in its own directory, and flipctl does that
itself: `app::install` runs cargo for a Rust app whose binary is missing or stale,
streaming the output to a log screen. Two things make that work from
`/usr/share/flipctl/apps/<app>` rather than only from a checkout, and both are easy
to undo by accident:

- The deploy installs `crates/{flipctl-app,flipper-ui,flipper-tokens}` and
  `third_party/flipctl-fonts` under `/usr/share/flipctl`, because an app's manifest
  asks for `../../crates/flipctl-app` and `ui/fonts.slint` imports the TTFs from
  `../../../third_party`. The installed layout mirrors the repository for exactly
  those paths.
- `flipper-ui` declares `version`, `edition` and `license` literally rather than
  inheriting them from the workspace. Installed there is no workspace root above
  it, and inheritance fails with "failed to find a workspace root" before cargo
  reads a line of source.

## Architecture

**tokens.toml is the source of truth and it is generated into two languages.**
`crates/flipper-tokens` turns it into `theme.rs` (`flipper_ui::theme::{color,
metric, radius, timing, count}`) and `theme.slint` (`FlipperTheme.*`, imported as
`@theme`). Add a value there, never in a component. `[metric]` and `[radius]`
become Slint `length`; `[count]` stays `int` deliberately, because Slint refuses to
multiply a length by a length and that separation is what catches it.

`write_if_changed` in the generator is load-bearing, not an optimisation:
rewriting `theme.slint` unconditionally stamps it after cargo's own completion
marker and cargo then reruns the build script forever, an 83-second recompile on
every build.

**One `Window`.** `ui/root.slint` is it; the platform hands out a single
`MinimalSoftwareWindow` and a second `Window` component would silently steal the
panel. Screens are property blocks on `Root` selected by the `Screen` enum. Adding
one means: a `struct` per row kind, a `*Body` component, a property block on `Root`,
an enum variant, an `apply_*` function in the binary, and a branch in the key
handler. `ui/wifi.slint` plus `wifi` in `bin/flipctl/src/main.rs` is the fullest
worked example; `boot.slint` plus `src/boot_menu.rs` is the other.

**Geometry and measurement in Rust, drawing in Slint.** This is the line that
matters. Slint cannot measure across a model, cannot accumulate a running total in
a loop, and cannot be built by a test without a window. So a screen's rows and
every number that positions them are plain Rust in `flipper-ui` (`wifi::Row`,
`boot_menu::View`), unit-tested there, and the binary maps them onto the Slint
structs at the boundary. Row `y` and `h` are given, not computed in the component.
When a component does need a string's width, measure it with a hidden `Text` and
take `(ref.width - 1px)`.

**Live data has three shapes, and picking the wrong one is the usual mistake:**

| Shape | Use | Cost |
|---|---|---|
| `StatusSource` | the status bar. sysfs only, read **inline on the render loop** at 1Hz | must stay syscall-only. Never shell out here |
| `Watch<T>` | a screen's own poller: a thread, a cadence, `get()`/`take_dirty()` | dropping it stops the thread. That is the whole API |
| told, not asked | `net.rs` (nmcli monitor), `route_watch.rs` (rtnetlink) | a thread parked until the kernel or NM speaks |

A poller belongs to the screen that opened it. The loop drops it when the screen is
no longer the one that owns it, so do not add cleanup to each exit path; the deck
counts as still being on the screen underneath it. Prefer being told over polling:
`netlink.rs` has the socket and the attribute walkers, and adding a subscription is
a protocol number and a group mask.

**The render loop shows a press before acting on it.** `Flash` holds the key for
`press_flash_ms`, and the action fires when the flash expires, so the inverted row
is drawn at least once. Do not act directly in the key handler for anything that
changes screen.

## Invariants that fail silently

1. **Every pixel must hold a design-token value.** `tests/render.rs` and
   `tests/wifi.rs` render each screen and fail on any grey absent from
   `tokens.toml`, naming coordinates. Two exemptions: sprite boxes (the icons are
   6-bit greyscale so their edges survive) and a modal scrim, where only the exact
   75%-white composite of a token is allowed.
2. **Font size is always 16px** and `SLINT_FONT_SIZES` is pinned to it. The pixel
   fonts rasterise with zero partial coverage at 16 and antialias at 15 or 17.
3. **`font-family` takes the family name.** `"HaxrCorp 4090"`, not
   `"HaxrCorp 4090 FlipCTL"`. A wrong name substitutes a fallback face with no
   warning and antialiases every glyph.
4. **No `border-radius`.** One corner rule, the 45-degree chamfer, generated for
   any radius by `SelectorFrame` in `ui/frame.slint` (which also does per-corner
   flags, the drop shadow and the fill). `docs/inventory.md` explains why
   `drawRoundRect`'s tighter step was retired.
5. **Slint drops a trailing space when it measures a string.** A hidden-`Text`
   measurement of `"Connected to: "` lands two pixels short. Measure that kind of
   string in Rust with `flipper_ui::font::{TITLE, ROW, ROW_ACTIVE}` and pass the
   offset in.
6. **The fonts are printable ASCII, 32..126.** No ellipsis, em dash or bullet: an
   unknown glyph draws as `?`. Truncate with `..`, show an unknown value as `-`,
   mask a password with `*`.
7. **The panel is 8-bit greyscale.** A token whose channels differ is shifted by
   `drm_fb_xrgb8888_to_gray8`; `tests/tokens.rs` rejects non-greys.

## Where truth lives

The prototype is `../flipperone-testing/fake-flipctl2`. **Its source is normative,
its own CLAUDE.md is not** and has drifted in a dozen places; `docs/inventory.md`
catalogues the drift and every deliberate divergence this port has made. When you
change behaviour away from the prototype, record it there.

## Verifying UI work

Build the real render and look at it. A structural argument about geometry is not
evidence, and neither is a passing test that nobody has eyeballed.

On the device, the browser view streams the panel: `curl -s --max-time 3
http://<device>:8899/stream` gives chunks of an 8-byte little-endian header
(x, y, w, h) followed by `w * h` grey bytes, which decode straight into a PNG. That
is the real framebuffer, so it settles arguments about what is actually on glass.

For anything that needs navigating, deploy and ask the user to look rather than
driving the keys remotely.
