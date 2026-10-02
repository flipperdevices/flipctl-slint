# Design system inventory

Phase 0 deliverable. The normative record of what the Flipper One UI looks like,
extracted from `fake-flipctl2`.

`tokens.toml` is the machine-readable half of this document. Every value in it
was read out of prototype **source**, never out of
`fake-flipctl2/fake_flipctl2_CLAUDE.md`, because that document has drifted from
the code in a dozen places. The drift is catalogued below so nobody re-derives
values from it by mistake.

## Rendering invariants

The panel is a 256x144 8-bit greyscale device: `flipper-one-display.c` advertises
`DRM_FORMAT_XRGB8888` only and converts what it is handed with
`drm_fb_xrgb8888_to_gray8`.

1. **Every pixel holds a design-token value.** Enforced by
   `tests/render.rs::rendered_frames_use_only_design_tokens`, which renders each
   screen and fails on any grey level absent from `tokens.toml`, naming the
   offending coordinates. The reference list screen resolves to exactly four
   values: `#000000`, `#ffffff`, `#cccccc` (divider), `#999999` (status text).
   One exemption, and it is arithmetic rather than a free pass: a modal's scrim is
   75% white over whatever was behind it, and the prototype dims the whole canvas
   including the status bar, so the dimmed tones cannot be tokens. `tests/wifi.rs`
   allows the exact composite of a token and nothing else, which still catches an
   antialiased corner or a fallback face.
2. **No antialiasing.** Consequence of (1). The three practical causes of a
   violation are a `border-radius` corner, a `Text` at a fractional offset, and a
   `font-family` that silently fell back to a substitute face.
3. **Integer pixels only.** No fractional layout, no scale factor.
4. **Font size is always 16px.** The FlipCTL fonts sit on a 64-unit design-pixel
   grid at 1024 units/em, so they rasterise with zero partial coverage at 16px
   and antialias at 8, 15 or 17px. `SLINT_FONT_SIZES` is pinned to 16 in
   `build.rs`.
5. **`font-family` takes the family name, not the full name.** `"HaxrCorp 4090"`,
   `"Busy9px"`, `"Born2bSportyV2"`. Passing `"HaxrCorp 4090 FlipCTL"` substitutes
   a fallback face with no warning and antialiases every glyph.

## Fonts

All 282 printable-ASCII glyph shapes in the prototype's packed 1bpp tables
(`haxrcorp16.js`, `busy9.js`, `born2bsportyv2.js`) are **byte-identical** to the
`flipctl-fonts` TTFs rasterised at 16px and thresholded at >= 128. At that size
the threshold is a no-op, so the JS tables and the TTFs are interchangeable. The
tables are vendored as Rust arrays by `tools/js-font-to-rust.py` and serve as the
measurement oracle; Slint renders from the TTFs.

| Role | Family | Frame | Used by |
|---|---|---|---|
| title | HaxrCorp 4090 | 13 rows x 16 cols | status bar, titles, dialogs, message box |
| row | Busy9px | 16 x 16 | `MenuLine` label and status, DEFAULT state |
| row_active | Born2bSportyV2 | 18 x 16 | `MenuLine`, SELECTED and PRESSED states |

The font swap on selection is load-bearing: a selected row changes typeface, not
just weight.

## Elements that are not rounded rectangles

Three prototype components are built from hand-placed pixels and cannot be
expressed with `border-radius`. Transcribed in `ui/frame.slint`, with
`paint.rs` holding the same geometry as the test oracle.

**`MenuSelectorFrame`** (`component-library/MenuSelectorFrame.js`)
- corners are straight 45-degree stairs, `(x + i, y + r - 1 - i)` for `i` in `0..r`
- a 1px drop shadow at `y + h` and at `x + w`, both *outside* the frame, each
  inset by the radius and one pixel longer than the edge it shadows so the two
  runs meet at the bottom-right stair
- two extra pixels at `(x + w - 2, y + h - 1)` and `(x + w - 1, y + h - 2)`
  weighting the bottom-right cut
- per-corner enable flags, so a frame can round any subset of its corners
- the fill is chamfered along the same diagonal as the stroke, row by row, so cut
  corners stay transparent

**Bottom-bar soft buttons** (`canvas.js` `drawMiddleButton` and friends)
- four-step stair: the fill steps in 3, 2, 1, 0 pixels over the first four rows
- two explicit border pixels per rounded corner, at `(2, 1)` and `(1, 2)`
- no bottom border; the button sits flush against the bottom of the panel
- outer slots run flush to the screen edge, losing both the stair and the
  vertical border on that side

**`MessageBox`**, keyboard container, `PopupMenuLeft` - same situation, not yet
transcribed.

Note that `drawRoundRect` / `drawRoundFrame` use a **different** corner treatment
from `MenuSelectorFrame`: two pixels at `(2, 1)` and `(1, 2)` for `r = 3`, versus
the selector's three-pixel diagonal. Both are in use. Dialogs and popups take the
`drawRoundFrame` form.

## Where the prototype's CLAUDE.md is wrong

Verified against the code on 2026-08-19. Code wins in every row.

| Claim in the doc | Actually in the code | Source |
|---|---|---|
| divider `#EDEDED` | `#CCCCCC` | `apps/menu.js` `DIVIDER_COLOR` |
| "Unselected `#EAEAEA`" | no such colour anywhere | - |
| `TEXT_DRAW_Y = 2`, cap top at y+5 | `3`, cap top at y+6 | `MenuLine.js` |
| `STATUS_PAD_R = 3` | `5` | `MenuLine.js` |
| `CONTAINER_X = 16` | `5` | `apps/menu.js` |
| `SELECTOR_X = 10`, `W = 232` | `4`, `244` with scrollbar / `247` without | `apps/menu.js` |
| fonts named `HaxrcorpFont16`, `BusyFont9`, `Born2bSportyV2Medium` | `HaxrCorp4090FlipCTL`, `Busy9pxFlipCTL`, `Born2bSportyV2FlipCTL` | `js/*.js` |
| status bar 0-11px, 2px divider gap, clock centred | `STATUS_BAR_H = 13`, no clock rendered | `ui.js` |
| disabled = `#CCC` ground, `#999` text | `drawMiddleButton` keeps a **white** ground and greys only outline and text; `drawLeftButton` / `drawRightButton` do use `#CCC` | `canvas.js` |

That last row is a genuine inconsistency between components, not a documentation
error. It needs a decision: pick one disabled treatment.

## Decisions, resolved 2026-08-19

**No grey that is not a token.** The selection frame keeps its raised look and
every rounded element is transcribed rather than approximated with
`border-radius`. The reference list screen renders in exactly four values.
Enforced by `tests/render.rs`.

**One corner rule: the 45-degree diagonal.** canvas.js carried two corner tables
for the same radius; `drawRoundFrame`'s tighter two-pixel step is retired and
everything uses `MenuSelectorFrame`'s diagonal, generalised to any radius as
`(i, r - 1 - i)` for `i` in `0..r` with straight edges beginning `r` pixels in.
That single rule reproduces the r=3 selector corner and the r=4 soft-button stair
byte for byte, which `tests/paint.rs` pins. Dialogs, `MessageBox` and
`PopupMenuLeft` change slightly when they are built; the selector does not change
at all.

**Disabled: outline only.** White ground, `#999999` outline and label. The
`#cccccc` ground the left and right slots used is retired, so all five slots now
match. Recorded as `rule.disabled = "outline-only"`.

**Overflow: wrap to a second line.** Recorded as `rule.overflow = "wrap"`, applied
wherever there is vertical room: dialogs, `MessageBox`, titles, detail screens.
`MenuLine` is the one place it cannot apply, because a 20px row cannot hold two
16px lines and growing the row would make list height variable and break the
five-row viewport; a label longer than its row truncates there. This matches what
fake-flipctl2 actually does, since its wrapping commits targeted dialog text and
the booting profile name, never menu rows.

**Soft-key row: esc, view, power, edit, run**, left to right. This confirms
`drivers/input/misc/flipper-one-input.c` (`KEY_Z`, `KEY_X`, `KEY_C`, `KEY_V`,
`KEY_B`) and retires `fake-flipctl2/js/input.js`, which named two of them `edit`
and `del`. `src/key.rs` is the single mapping; `tests/tokens.rs` asserts the order
and that no `del` key exists.

## The Wi-Fi page, ported 2026-09-02

`apps/wifi.js` and its five server endpoints, transcribed into `ui/wifi.slint`,
`src/wifi.rs` and the state machine in `bin/flipctl`. Four places where the port
does not do what the prototype does, and why:

**Truncation ends in `..`, not an ellipsis.** `ellipsizeTo` appends U+2026 and the
panel's fonts are printable ASCII only, so the prototype's own glyph table
substitutes `?` for it: a cut SSID reads "MyNetwo?" on the device. `..` is what
this port already truncates with, in `detail::elide`.

**A missing value reads `-`, not an em dash.** Same reason: the em-dash character
wifi.js uses for an unknown Auto join state is not in the table either.

**Auto join is written, not only shown.** wifi.js flips its own copy and leaves
`nmcli connection modify` for later, so the row forgets on reopen. Here it is
written, detached and optimistic like the radio toggles.

**No touchpad scroll on the settings page, and no LED.** The prototype drags that
modal with `/api/touchpad/xy` and pulses the Wi-Fi LED blue while a join runs.
Neither source exists on this side: flipctl has no touchpad input path, and the
LEDs are driven by the prototype's server. Keys scroll it instead.

One prototype bug is not reproduced. The connect prompt keeps its keyboard on
screen under a wash while nmcli works; here the page comes back with the spinner
over its own list, which is the same state the saved-profile join already showed,
and a refusal reopens the keyboard with nmcli's own reason under the field.

## Internet radio, ported 2026-09-03

`js/apps/internet_radio.js`, transcribed into `apps/radio` plus a widget both it and
flipctl's own screens can use: the settings row with a value chip is
`ui/dropdown.slint` and `src/dropdown.rs` in flipper-ui, and only the page that
stacks four of them belongs to the app.

The prototype is a scene that asks a server to do everything: `POST /api/radio/play`
runs mpg123, `/api/radio/status` says whether it is still alive, `/api/sound/volume`
writes the codec mixer, `/api/sound/outputs` lists the outputs and
`/api/radio/install` installs the decoder. There is no server here, so each of those
became something the app does itself, and that is where every divergence comes from.

**The player is mpv, and the app owns it.** mpg123 decodes the same streams in a
tenth of the size and cannot be asked anything while it does it. mpv takes a JSON
command socket, which is where three rows get their answers: the volume it applies
to a running stream, the output it can move one to, and the station's now-playing
title. It also dies with the app (`PR_SET_PDEATHSIG`), where the prototype's server
kept mpg123 playing after the user left the scene.

**Titles are transliterated, not shown as question marks.** The panel's fonts are
printable ASCII, which the prototype worked around by writing every station name in
ASCII by hand. A now-playing title is not ours to write: Nashe Radio sends Cyrillic
and Belgrade's stations send Serbian in either alphabet, and both arrived on the
panel as a row of `?`. `font::ascii` now turns the Latin letters with marks on them
and the whole Cyrillic alphabet into the letters underneath, so "ДДТ - Что такое
осень" reads "DDT - Chto takoe osen" and "Đorđe Balašević" reads "Djordje
Balasevic". It lives in flipper-ui rather than in the app because every screen that
shows a string from outside the device has the same problem, the Wi-Fi page's SSIDs
included; only the radio uses it so far.

**A city and two stations more than the prototype.** Belgrade, with Cool Radio and
Naxi Radio, and drum and bass in London: Jungletrain and Rage FM. All four were
found in the same directory the prototype's Spreeradio URL came from and probed the
same way before being written down, and two of the four send a title.

Two candidates were dropped for sending one that is not a title. Rinse FM answers
`Now Playing info goes here` and Flex FM answers `FOLLOW @FLEXFMUK`, and a page
that cannot tell those from a track would put either on the panel as if it were
one. There is no filtering for it: a station either says what is playing or says
nothing, and nothing is what the fallback to its own name is for.

**Now playing is the station's own title, not just its name.** New here, and the
reason for choosing mpv: a line under the last row reads `Playing: <icy-title>`
while the station sends one and falls back to its name when it does not. It went in
the title bar first, beside the app's name, which is where the prototype puts the
station name; moved under the rows because a title is the longest string on this
screen and the bar could only spare 147 of the page's 245 pixels for it. Non-ASCII in a title becomes
`?` in Rust rather than at the glyph table, so what is measured is what appears.

**The volume is the stream's, not the machine's.** The prototype's slider wrote the
NAU8822 mixer through `amixer`, so it moved the whole device's output. Here it is
mpv's own volume: it moves this stream and leaves the user's system volume alone.
Nothing else on the panel is playing, so the two look the same on the device, and
only this one cannot surprise somebody by turning the desktop down.

**The outputs come from mpv, and the first one is the sink flipctl pinned.** The
prototype's server split the codec into Speaker and Headphone and hid Loopback.
mpv's own enumeration of PipeWire sinks is what the player will actually accept, so
that is the list, with `Default` for passing it no device at all, which leaves the
app on the panel's speaker that `wl.rs` pinned for it.

What the row shows depends on when the television was plugged in, which is a device
quirk rather than an app one and worth writing down. Measured 2026-09-03:

  * With nothing attached the card is left in its `off` profile and there is one
    sink, `alsa_output.platform-sound.stereo-fallback` ("On-board NAU8822 Analog
    Output"). The row then has `Default` and that, which are the same output.
  * With a television attached when wireplumber starts, the card takes its UCM
    `HiFi` profile and `alsa_output.platform-hdmi-sound.HiFi__HDMI__sink` appears.
    It plays: the PCM reaches `RUNNING` with `hw_ptr` advancing, and wireplumber
    makes it the default sink, which the app is immune to because flipctl pins the
    panel's speaker for it.
  * Attaching one afterwards changes nothing until wireplumber restarts, even
    though the ELD is populated the moment the cable goes in (it names the
    monitor). Nothing tells ALSA the display arrived, so a card probed with nothing
    attached stays off. That is a kernel-side gap in the HDMI codec's jack
    reporting, not something the app or wireplumber can see.

The app therefore lists whatever sinks exist when it starts, which is the honest
answer to a question whose answer changes.

Left alone deliberately, 2026-09-03: attach the television before the machine boots
and HDMI audio works. The two cheap ways to paper over the hotplug case were both
tried and rejected as not worth their cost -- a udev rule on the DRM change event
that restarts wireplumber glitches whatever is playing, and forcing the card's
`pro-audio` profile in a drop-in gives an always-present sink at the price of
bypassing its UCM profile. The fix that is actually missing is jack reporting from
the HDMI codec, in the kernel.

**A stream is judged by whether sound comes out, not by a timer.** The prototype
could only ask whether mpg123 was still running, so it gave a stream 3.5 seconds to
prove itself and treated anything else as a failure. mpv answers a better question:
`time-pos` is the position of the audio being played, so it appears when sound does.
Measured on the two ways a station can fail: a URL that answers with something
unplayable ends mpv inside five seconds, which is the dialog, and a host that cannot
be reached leaves mpv up with no position for as long as its own timeouts take,
which that line reports as `Connecting...` because nothing has failed yet. A stream
that dies after it was playing stops without a dialog: the button going back to Play
is all there is to say about it.

**No install dialog.** The prototype's three-state modal exists because its server
had no package manager integration. `app.toml` declares mpv and flipctl installs it
before the app is launched, so the case the modal covered cannot arrive.

**The failure dialog is flipctl's own.** Same wording as the prototype, "Stream
error" over "Could not start `<station>`", in `Modal` with `Ok` on the right soft
key rather than the prototype's centred 150x70 frame with a stacked button.

**A truncated value ends in `..`, and `Close` is on the left soft key.** The first
for the reason every other screen has it. The second is an addition: the prototype
draws only its right-hand Play button and leaves Back undiscoverable.

**Nothing is remembered between launches.** The prototype parks the city, the
station and the volume in a module-level object so re-entering the scene finds them,
which works because the scene is torn down and the page is not. An app here is a
process: closing it ends the process, and the state goes with it.

## The browser, ported 2026-09-28

`js/apps/browser.js` (cf09c8c, "fake browser concept"), transcribed into
`apps/browser`. The prototype pans and zooms over a screenshot; here the screenshot is
a live page, from Chromium running headless and screencasting over the DevTools
protocol on a pipe. The numbers are the prototype's: x1 to x8 at 300 pad units a
doubling in steps of 0.05, a tick of the motor every 0.1 and a thump at either limit,
the 4px crosshair with its white halo, and the 64px minimap. Where the prototype
stops, the port had to go on, and that is where each divergence comes from.

**The page is seven panels wide.** Chromium lays out at 1792x1008, the shape of the
prototype's 1790x1008 mockup rounded to a multiple of the panel, so x1 fills the panel
exactly and x7 is one CSS pixel to one panel pixel.

**Shrinking is a summed-area table, not a mip chain.** The prototype halves its image
repeatedly and draws from the nearest level so that every source pixel is averaged.
A summed-area table gives the exact mean of any rectangle in four lookups, so each
panel pixel is the average of the page pixels under it at every factor, not only at
the mip levels, and a frame costs one pass to prepare.

**Zoom follows the finger sideways, not up and down.** The prototype zooms on the
vertical travel, up to magnify. On the device that was asked to change on first use:
with PTT held, right magnifies and left shrinks, at the same 300 units a doubling.

**The minimap sits bottom right, and only while the view moves.** The prototype
puts it bottom left and shows it whenever the zoom is past x1. Asked for on the
device: it sits in the bottom-right corner, outlined black with white outside that
and a pixel of white inside it, so the view's frame never merges with the outline at
the page's edge, and in by those three pixels so all four of its sides show, and it comes up
when the pad or the arrows move the view, stays while PTT is held, and goes down
three seconds after the last move.

**The halo is kept as the prototype draws it.** It runs one pixel past the left and
top arms and stops level with the right and bottom ones. That looks like an
off-by-one, but it is what the design shows, so it is transcribed rather than
corrected.

**New: Ok clicks, the arrows pan, Edit types an address, Back goes back.** The
prototype has none of them yet. Ok clicks the page point under the cursor. The arrows
move the cursor 16 panel pixels a press, as the pad would; with the cursor already
against the page's edge, a press further that way scrolls the page by 80% of what is
visible instead, so each screenful overlaps the last. They scrolled on every press at
first, which on the device read as the arrows doing nothing whenever the page had
nowhere to go. Edit opens flipctl's own keyboard on the current address: a bare host gets
`https://`, and words become a DuckDuckGo search. Back goes one page back and leaves
from the first; Escape always leaves. A drag of the cursor also moves the page's
pointer, so hover states show.

**A click in a text field opens the keyboard on it.** New, and not in the prototype.
After Ok the app asks the page, off the thread that draws, whether the focus is now
in a text input, a textarea or an editable element, looking through shadow roots and
same-origin frames. If it is, the keyboard opens titled with the field's label,
placeholder or name, and with its text. Done selects the field's contents and types
over them with `Input.insertText`, so the page hears ordinary input events rather
than having its value set behind its back. Done on unchanged text writes nothing,
since the panel shows the value in ASCII and that is not always the value. A
password is shown as typed: the keyboard has no masking yet, and flipctl's own
password entry has none either. Nothing is submitted: Done fills the field and the
page's own button sends it.

**New: the strip, with tabs, instead of soft keys.** Asked for on the device: no
soft keys, and the address reached by pushing the cursor up. At the top of the
site, which is the page scrolled to its top (the screencast frame carries the scroll
offset) with the view showing that top, a 32px strip is across the panel and the page
is pushed down under it, not covered; once the view leaves the top, by scrolling or
by panning down while zoomed, the strip goes and the page has the whole panel. Up
with the cursor on the top edge there, or a drag that carries on 12 panel pixels past
it, moves the highlight into the strip, which is laid out as Chromium's: a grey tab row with the tab
on screen white and a cross on it, a + after the last, then back, forward and reload
as 7x7 icons beside the address in the keyboard's grey field. The arrows and the pad step the highlight from item to item, never
onto a cross from the other row; Ok does what it is on; Down off the address or Back
comes back to the page. The strip first came down only when asked for and went away
again on Down, and on the device the first press anybody made was Down, meaning to
reach the address, which put it away. It was then made permanent, with the page
laid out at 1792x784 under it, and then asked to belong to the top of the site only,
which is where it is now and why the layout is back to 1792x1008. Each tab is a target of its own in the one engine, and only the
tab on screen is screencast. Titles are asked for with `document.title` when a tab
loads, because the engine's target events carry only the title a page had when it
was created. Closing the last tab sends it home rather than leaving nothing on screen.

**A tab is a phone until it is told otherwise.** Asked for so that a captive
portal's sign-in page comes up in its phone layout. A tab is laid out at 360x203, the
narrowest phone in use and the width sites are tested at: 16px body text is then 6 panel
pixels in x-height at x1, a pixel more than the panel's own font, where 512 made it 4.
It says it is an Android phone in
its User-Agent and its client hints. The engine is started with the phone's User-Agent
and size, because a tab's first request goes out before a tab of it can be told
anything. A phone icon beside reload switches the tab to a desktop, 1792x1008 and a
desktop's User-Agent, and reloads it; the icon is then a monitor. A page without a
viewport meta tag is laid out 980 wide in mobile mode, as a phone lays it out.

**The engine does not say it is headless.** DuckDuckGo answered a search on the
device with a captcha. Headless Chromium puts HeadlessChrome in its User-Agent and
its client hints and sets navigator.webdriver, so it is started with the User-Agent
desktop Chromium of the same major version sends on Linux and with
AutomationControlled off. It is a launch flag rather than a per-tab override because
a tab created blank and sent on to its address afterwards never sent a picture, and a
tab created on its address has made its first request before it can be told
anything. The captcha could not be made to come back from the device or the host
with or without this, so it cuts the obvious signals rather than being a proven fix.

**The pad is the app's own.** The compositor has no pointer, so nothing reaches a
hosted app's pad unless the app opens it. The browser reads it on a thread of its own
through `TouchpadSource`, which `flipctl-app` now exports behind its `device`
feature, together with `Haptic`, the keyboard and `Key::flipper`.

## System info, added 2026-10-01

`menu.js` lists "System info" first under Settings and opens nothing from it: there is
no scene behind the row. This one is ours, a detail screen like Battery info, with no
prototype to transcribe.

It shows what the machine is (model, hostname, SoC, CPU, CPU serial, memory), what it
runs (OS, build, git, profile, when the profile was made, kernel, when that was built,
flipctl's own commit), the UFS (part, size, version, link, life used, health), the
SD card, the battery, the radios, every hardware address, and uptime. Long, so it
scrolls, and a value too wide for its row wraps onto full-width lines under its
label, per `rule.overflow`. Everything is read from files and two unprivileged
ioctls, on a two-second `Watch`; nothing shells out.

**The right soft key jumps a section and names it**: Software, UFS, SD, Battery and
so on, then Top from the last screen.

**QR shows the page as a QR code, full screen.** Plain text, one `label: value` a
line: Apple's Camera acts only on the payloads it knows (URLs, geo, SMS, mail, tel,
contacts, events, Wi-Fi) and says "No usable data found" for text, which a scanner
app or Google Lens shows. The code is always version 29, 133 modules a pixel each,
with `qr_icon` modules left white in the middle for the icon. It carries less than
the page (no SD card, radios, or anything that changes by the minute) so the text
fits error correction Q, which pays for the icon's square with room to spare.

**The SD card's ratings come from its SD Status register** (speed class, UHS grade,
video class, app performance class), the way the SD spec lays them out. The bus mode
is only in debugfs, so it is not shown.

**Memory is what is fitted**, from the device tree, in binary gigabytes, beside what
is in use. `MemTotal` is short by the firmware's carve-out and what the kernel keeps.

**The CPU serial is U-Boot's**, worked out from the OTP's cpuid the way
`rockchip_cpuid_set` does it, so it agrees with the SSH banner and the access point's
SSID. The OTP is world-readable; no tool is run.

**The Wi-Fi address is the card's own**, from `ETHTOOL_GPERMADDR`: NetworkManager
randomises the one it scans with, so `address` says nothing about the card.
Bluetooth's comes from `HCIGETDEVINFO`, since sysfs does not carry it.

**`BUILD_GIT` is shown as the image wrote it**, which is with its `@`, `:` and `-`
stripped by the build's `tr -dc '[:alnum:][:space:]'`. The clean form is not kept
anywhere on the device.

## The idle screen's addresses, revisited 2026-10-01

`drawEthCard` gives each interface a two-line card, one IPv4 and one IPv6, and the
port showed only hardware ports with a carrier. Every address of every interface but
loopback and dummies is shown now, one line each, so a card is as tall as its
interface has addresses and the list scrolls a line at a time. A two-address card
keeps the prototype's 26px pitch.

## Poll cadences, revisited 2026-09-03

Each detail screen was given its prototype scene's own interval. Two of those did
not survive contact with what they cost:

**5G Modem: 500ms to 2s.** `modem5g.js` polls at 2Hz and each poll is four
subprocesses, three `mmcli` and one `qmicli`. Nothing on the page moves that
fast: the operator and the access technology change on network events, and the
signal is whatever ModemManager last cached, refreshed on its own schedule, so
asking twice a second returns the same number again. Two seconds is the Ethernet
page's cadence and a quarter of the processes.

**A screen's pollers now stop with the screen.** They were dropped at each way out
and only the Escape key ever did it, so leaving a detail page through the app
switcher left its watch running for the rest of the session -- the modem's four
subprocesses among them. The loop drops them when the screen is no longer the one
that owns them, which catches every exit including the ones that never touch a
key. The deck counts as still being on the screen underneath it, since it is an
overlay the user returns from. An app in front does not: it leaves the screen enum
alone by design, so a page whose app was launched over it keeps polling.

The status bar (1s), the idle screen's sensors (1s, raised from 5s: three sysfs
reads that only redraw when a value moves, and a temperature reading that lags five
seconds behind the fan is worth less than the reads cost) and its addresses (30s)
are left polling. Those are sysfs reads and `getifaddrs` -- microseconds, no
processes -- and keeping them warm is what makes the root screen correct the moment
you land on it rather than up to thirty seconds later.

## Soft-button strip, approved 2026-08-19

Measured pixel by pixel from the Figma export `soft_button_bar`, which is kept as
`tests/reference/soft_button_bar.png`.

| | value |
|---|---|
| button width | 48 |
| gap | 4 |
| pitch | 52, slots at 0, 52, 104, 156, 208 |
| height | 14 |
| corner | 3px 45-degree diagonal, top corners only |
| bottom border | none, the strip runs to the panel bottom |
| label | centred, `y + 2`, caps on strip rows 4..10, 8-character budget (37px) |

`5 * 48 + 4 * 4 = 256` exactly, so the row fills the panel and both outer edges
land flush. This replaces canvas.js's arrangement, which tiled from the left with
a 2px gap and then anchored the right button at `screenW - w`, leaving a 10px hole
before the last slot.

**The outer two slots are special.** Slot 0 has no left border and its top edge
starts at x0; slot 4 has no right border and its top edge runs to x255. The three
inner slots are outlined on both sides. The set of columns carrying a border down
the straight body is therefore `47, 52, 99, 104, 151, 156, 203, 208`, and
`tests/render.rs` asserts exactly that.

The strip starts at y130, so the list occupies y26..y129: five 20px rows plus four
dividers is 104px, and `26 + 104 = 130` meets the strip with no wasted row.
menu.js used 25, which left row 129 unused.

Height is the one place the Figma export and the shipping device disagree: the
export is 17 rows, the device is 14, and **14 is the decision**. The reference
test therefore compares rows 0..3 exactly and then the border-column set, both of
which are height independent; replace the reference with a 14-row export to
compare the whole strip again.

## The keyboard's touchpad, ported 2026-09-07

`keyboard_test.js`'s scheme, taken as it stands: a stroke moves the selection by
how far the finger has travelled since it landed, never to where the finger is,
because the pad sits beside the screen and no point on it means a key. Row -1 is
the field, where the column delta becomes the caret; the tab strip below is a
dead end, since the number row is the D-pad's way in. Lifting does not press.

The input is the prototype's too: raw `ABS_X` / `ABS_Y` / `BTN_TOUCH`, flushed on
`SYN_REPORT`. Its server transforms nothing, and neither does `TouchpadSource`.
The comment there about the Y axis spanning "~400 units" is the practical span of
a swipe; the hardware reports 0..800.

Two deliberate divergences, both about the same problem. With the prototype's
`TP_X_UNITS_PER_STEP` of 32 a column costs 64 raw units, 6% of the pad's width,
against a row's 260, so **X is four times the more sensitive axis** and a swipe
meant to change row changed column as well. Reported from the device.

- **X slowed to 48**, so a column is 96 raw units. A practical sweep still
  crosses most of the widest row.
- **Y quickened to 90**, so a row is 180 raw of an 800-unit axis. At the
  prototype's 130 a row cost 260 and a full-height sweep crossed only 3.1 of the
  four rows, so the number row and the shift row could not be reached from each
  other in one stroke. Reported from the device as Y feeling slow against X.
An axis lock was tried for this and removed on the device: with X slowed and Y
quickened it fought a stroke that wanted to change direction more than it helped
a stroke that did not. A vertical swipe can still creep a column if the finger
wanders; slowing X is what keeps that rare.

A third divergence, in the other axis. The prototype's `_setSelection` carries the
column **index** across a row change, and our rows are ragged: the home and shift
rows lead with a 35px key where QWERTY leads with a 15px one, so index 4 on row 1
sits at x=102 and index 4 on row 2 at x=87. Measured, a straight drag between
those rows stepped one key sideways. The pad now carries the **position** and
takes `closest_col` on the row it lands on, which is what the D-pad's
`snap_to_row` has always done. Reported from the device as up and down not suiting
the layout.

`Math.round`'s tie behaviour is kept: ties go toward positive, so the first tick
downward comes at exactly half a step and upward has to pass it. Asymmetric, and
the prototype's.

## Which source is normative

This document treats **fake-flipctl2 source** as normative for measurements and
**Vlad's Figma exports** as the design authority, and on the soft-button height
those two disagreed, with the prototype winning. That makes the other numbers taken
from prototype source worth one deliberate pass rather than element-by-element
discovery: `MenuSelectorFrame`'s drop shadow, the 20px `MenuLine`, the `#CCCCCC`
divider, `TEXT_DRAW_Y = 3`, and the disabled-state treatment.

A design export saved as a test fixture is the only kind of golden here that can
catch an implementation being self-consistently wrong; every other golden is
generated from our own output. Exports dropped in at 1x, or a clean integer
multiple, can each become one.

## Open questions

None blocking. Still to decide when the components are built: whether
`PopupMenuLeft`'s `#d0d0d0` shadow edge survives the no-grey rule, since it is a
token but a very light one.

## Renders

In `docs/inventory/`, all 8-bit greyscale PNGs at 256x144 unless the name says
otherwise. These are the exact bytes the panel receives.

| File | Content |
|---|---|
| `list-no-grey.png` | reference list screen, 4 token values, zero antialiasing |
| `list-no-grey-4x.png` | the same at 4x for reading |
| `selector-menuselectorframe.png` | transcribed selector frame |
| `selector-border-radius.png` | `border-radius`, for comparison |
| `*-pressed.png` | press-flash variants |
| `compare-row-1x-2x-4x.png` | selector row, both treatments, three scales |
| `compare-corner-22x.png` | bottom-right corner at 22x |
| `compare-full-4x.png` | both full screens |
| `compare-grey-vs-none-3x.png` | before and after the grey was removed |

`tests/golden/list.png` and `list-pressed.png` are the committed goldens, along
with the Ethernet cards, the card row and ten Wi-Fi screens: the page with the
radio on and off, the visible list plain, pressed and still scanning, the saved
list, and one network's settings on each of its three kinds of row plus its own
loading state. Regenerate with `FLIPPER_UI_BLESS=1 cargo test --features screens`,
and look at what changed before committing it.

## Still to inventory

Status-bar badges (5G bars and tech label, wifi 7x7, ethernet 13x7, recording
dot, battery 16x9 with charging overlay and percentage), scrollbar with dotted
track, `PopupMenuLeft`, `DeleteConfirmDialog`, virtual keyboard (15x16 keys in a
250x77 container), `TextInputBox` / `InputField`, `MessageBox` and its tail,
roughly 35 static 6-bit greyscale icons and 10 animated vertical strips at 200ms
per frame.

`ResponsiveFrame`, `TabHeader` and the dotted scrollbar came with the Wi-Fi page:
the frame is `SelectorFrame` with per-corner radius flags, and the other two are
components of their own in `ui/frame.slint`.

## Driver gaps found on the device

Both are userspace-visible and neither is worked around in the kernel, per the
decision to keep kernel changes out of scope.

**No `dirty_fb`.** `flipper-one-display.c` sets `.fb_create = drm_gem_fb_create`
and no `dirty_fb`, so `DRM_IOCTL_MODE_DIRTYFB` returns `ENOSYS`. Every other
mipi-dbi tiny driver wires `drm_atomic_helper_dirtyfb`. `KmsSink` probes once and
falls back to re-issuing `set_crtc` with the same mode and framebuffer: mode and fb
are unchanged so the helpers set no `mode_changed` and there is no disable/enable
cycle, and `fo_crtc_check` calls `drm_atomic_add_affected_planes`, so the plane
lands in the commit and `fo_plane_atomic_update` writes the SPI buffer.
`KmsSink::flush_path()` reports which path was taken, so a driver that later gains
`dirty_fb` becomes visible instead of silently unused.

**No signal in sysfs.** `CONFIG_CFG80211_WEXT` is off in the kernel config, and it
is the wireless-extensions compat layer that creates both `/proc/net/wireless` and
the attributes in `/sys/class/net/<if>/wireless/`. So the file is absent and that
directory exists but is empty, which is how `status::read_wifi` came to pass its
`is_dir()` check and then read a quality of 0: the bar drew the empty signal icon
on a full-strength link. `CONFIG_MAC80211_DEBUGFS` is off too, so there is no
`stations/*/rssi` either, and mt76's own debugfs carries no signal. `nl80211.rs`
asks the kernel over netlink instead, as `iw` and NetworkManager do.

Turning WEXT on is a one-line addition to `minconfig-mainline` and would make the
sysfs path work again for everything, including a hosted app's own bar. `status`
tries the file first and falls back, so that change needs nothing unpicked here.

**The status bar's 5G block is drawn from numbers nobody fills in.** Latent, not
live: `read_status` hardcodes `access_tech: "--"` and `modem_quality: 0` while
`modem_available` comes from a real check for a `wwan`/`wwp`/`ppp` interface. No
board here has one, so the block is hidden and it has never shown. On a unit that
does have a modem it would appear permanently empty -- `filled` is
`ceil(quality / 20)`, so a quality of 0 lights no bars and all five render in
`bar_dim` -- with `--` for the label, and `modem_w` would push the wifi and
ethernet icons right to make room for it. The same shape as the wifi badge before
`nl80211.rs`: a badge drawn from a reading that is never taken.

Left as it is on purpose, and the note at `status::modem_present` says why: writing
it blind is what was being avoided. `sysinfo::modem` already reads all of it for
the 5G page, but at four subprocesses a read, so the status bar cannot call it at
1Hz -- it wants either a slow watch of its own or ModemManager's D-Bus signals, and
which of those is right depends on what the hardware answers. Also to decide then:
whether an unknown signal should draw five empty bars at all, or hold the block
back until there is a number for it.

**No damage clipping and no `DRM_FORMAT_R8`.** As recorded in the plan.
`fo_set_tx_buffer_data` hardcodes the clip to the whole framebuffer and
retransmits all 37152 bytes per commit, and the only advertised format is
`XRGB8888`, so userspace expands greyscale to 32bpp and the kernel converts it
straight back. Together these account for most of the ~4 ms of non-SPI commit
time.
