#!/usr/bin/env python3
# /// flipctl
# name = "Breathing LED"
# status = true
# ///
"""A breathing pattern on any combination of the four RGB LEDs.

Pick the LEDs, the colour, the period, the peak and the shape of the breath, and
press Start. Left and right adjust the selected row, All and None take every LED at
once, and leaving hands them back to the kernel's netdev trigger.

There is no pattern trigger in this kernel, so the breath is driven from here: one
line per frame down a pipe to a small writer running as root, since sysfs is root's
and a sudo per frame would cost more than the frame.

The LEDs sit on the same i2c device as the buttons, the MCU at 0-0069, on a 400kHz
bus. Four of them at 50Hz is 200 register writes a second, a few percent of it, and
the writer drops the ones that would not change anything: an LED that is not in the
pattern is written once and then left alone. Asking for much more than this is not
free -- queueing writes faster than the bus drains backs the driver up and takes the
panel with it.

Two decisions about the curve, and they are the whole of why this looks like
breathing rather than like a fading LED:

  * The shape is drawn in *perceived* lightness and converted to duty afterwards.
    The eye's response to light is close to a cube root, so a linear ramp of duty
    rushes through the dark half and crawls through the bright one. CIE 1931 gives
    the conversion: Y = ((L*+16)/116)^3 above L* 8 and L*/903.3 below it, which is
    the standard psychometric lightness curve and what makes a fade look even.

  * The shape itself is a gaussian with two different widths, narrow rising and
    wide falling, peaking about a third of the way in. Breathing is asymmetric --
    inspiration is quicker than expiration, an I:E ratio of about 1:1.5 -- and the
    wide falling side runs out before the cycle does, which leaves the rest of it
    dark: the pause between breaths. A sine has neither and reads as a pulse.
    Twelve breaths a minute is the low end of a resting rate, which is why five
    seconds is the default. Sine and Exp are kept beside it to compare against.

The LEDs are RGB565, so the bottom of the breath quantises: the last few percent of
the fade is steps rather than a slope, and a channel at 6 bits gets there first.
"""

import asyncio
import math
import subprocess

import flipctl
import slint

# Name on the device and what to call it on screen, in the order they sit along the
# top of the device.
LEDS = [("eth0", "ETH 0"), ("eth1", "ETH 1"), ("wifi", "Wi-Fi"), ("link", "Link")]
# Lime and Cyan are what the image gives the ports and the radio, so the tool can
# breathe a port in its own colour. The rest are the corners of the gamut, which is
# what a test tool wants: a channel that is dead shows as a colour that is wrong.
COLOURS = [
    ("Lime", (120, 255, 0)),
    ("Cyan", (0, 255, 255)),
    ("Green", (0, 255, 0)),
    ("Red", (255, 0, 0)),
    ("Blue", (0, 0, 255)),
    ("Amber", (255, 140, 0)),
    ("Violet", (160, 0, 255)),
    ("White", (255, 255, 255)),
]
PERIODS = [round(1.0 + 0.5 * i, 1) for i in range(15)]
PEAKS = list(range(10, 101, 10))
CURVES = ["Breath", "Sine", "Exp"]
# One frame per 20ms. Faster buys nothing: the panel is not drawing this, and at the
# shortest period a cycle is still 50 steps.
FRAME = 0.02

# Where the breath peaks and how wide it is on each side, as fractions of the cycle.
# Rising over three sigma from zero to the peak is 1.5s of a 5s breath; falling over
# three of the wider sigma lands at 0.8, so the last fifth of the cycle is the pause.
MU, SIGMA_IN, SIGMA_OUT = 0.30, 0.10, 0.17

ROWS = len(LEDS) + 4
COLOUR_ROW, PERIOD_ROW, PEAK_ROW, CURVE_ROW = ROWS - 4, ROWS - 3, ROWS - 2, ROWS - 1

# Slint has no way to build one array out of another, and a Python caller cannot
# fill ListItem's `image` field, so the rows are turned into ListItems by a literal
# each, which fills every field it does not mention with a default. Written out here
# rather than by hand so the count follows the row list above.
ITEM = """            {{ label: root.rows[{i}].label, value: root.rows[{i}].value,
              chevrons: root.rows[{i}].chevrons, at_start: root.rows[{i}].at_start,
              at_end: root.rows[{i}].at_end }},"""

PAGE = """
import { Shell } from "@app/shell.slint";
import { MenuBody } from "@flipctl/list.slint";

export struct BreathRow {
    label: string,
    value: string,
    chevrons: int,
    at_start: bool,
    at_end: bool,
}

export component App inherits Shell {
    in property <[BreathRow]> rows;
    in property <int> selected;
    in property <int> scroll;
    in property <int> arrow_pressed;
    in property <[string]> buttons;
    callback keyed(string, bool);
    key(text, down) => { root.keyed(text, down); }

    MenuBody {
        items: [
%s
        ];
        selected: root.selected;
        scroll: root.scroll;
        arrow_pressed: root.arrow_pressed;
        total: %d;
        real_frame: true;
        buttons: root.buttons;
    }
}
""" % ("\n".join(ITEM.format(i=i) for i in range(ROWS)), ROWS)

# Runs as root and does nothing but write, so the app keeps one sudo for its life
# instead of one per frame. Takes the four LEDs off their trigger, opens each
# multi_intensity once, and writes twelve numbers a line at whatever rate they
# arrive. Brightness stays at full: the breath is in the colour, which keeps the
# whole range of the driver rather than a brightness scaling on top of it.
#
# It also hands them back, rather than the app doing it. Whoever took the LEDs
# should be the one to return them: this exits on the pipe closing and on a TERM,
# and flipctl signals the whole group when an app is closed from the switcher, so a
# tool that is killed rather than left still gives the ports their lights back.
# Replaying the udev event is the whole restore, since the rules load the netdev
# trigger, bind each LED to its interface and set the colours.
WRITER = r"""
import os, signal, sys

LEDS = "/sys/class/leds/flipper-one:rgb:"
NAMES = ("eth0", "eth1", "wifi", "link")
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
fds = []
for name in NAMES:
    d = LEDS + name + "/"
    with open(d + "trigger", "w") as f:
        f.write("none")
    with open(d + "brightness", "w") as f:
        f.write("255")
    fds.append(os.open(d + "multi_intensity", os.O_WRONLY))
held = [None] * len(fds)
try:
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        fields = line.split()
        if len(fields) != 3 * len(fds):
            break
        for i, fd in enumerate(fds):
            colour = " ".join(fields[3 * i:3 * i + 3])
            if colour != held[i]:
                held[i] = colour
                os.pwrite(fd, colour.encode(), 0)
finally:
    os.system("udevadm trigger --subsystem-match=net --action=add; udevadm settle")
"""


def shape(curve: str, phase: float) -> float:
    """Perceived lightness, 0 to 1, at `phase` through one cycle.

    The gaussian is normalised per side so both ends of the cycle reach exactly
    zero: the two sides have different widths, so their tails are different heights
    and subtracting a single floor would leave a step at the wrap.
    """
    if curve == "Sine":
        return (1 - math.cos(2 * math.pi * phase)) / 2
    if curve == "Exp":
        # The other well-known breathing curve: a sine in the exponent, which is
        # peakier than a sine and symmetric, so it has no pause.
        raised = math.exp(math.sin(2 * math.pi * phase - math.pi / 2))
        return (raised - 1 / math.e) / (math.e - 1 / math.e)
    sigma = SIGMA_IN if phase < MU else SIGMA_OUT
    edge = (MU if phase < MU else 1 - MU) / sigma
    floor = math.exp(-edge * edge / 2)
    x = (phase - MU) / sigma
    return (math.exp(-x * x / 2) - floor) / (1 - floor)


def duty(lightness: float) -> float:
    """The luminance a perceived lightness needs, by CIE 1931. `lightness` is 0..1."""
    star = 100 * lightness
    return star / 903.3 if star <= 8 else ((star + 16) / 116) ** 3


class Breath:
    """The pattern, and the root writer it goes to.

    Phase is accumulated rather than derived from a clock, so changing the period
    stretches the rest of the breath instead of jumping to a new point in it.
    """

    def __init__(self) -> None:
        self.on = [False] * len(LEDS)
        self.colour = 0
        self.period = PERIODS.index(5.0)
        self.peak = len(PEAKS) - 1
        self.curve = 0
        self.phase = 0.0
        self.writer: subprocess.Popen | None = None

    @property
    def running(self) -> bool:
        return self.writer is not None

    def start(self) -> None:
        if self.running:
            return
        self.phase = 0.0
        self.writer = subprocess.Popen(
            ["sudo", "python3", "-c", WRITER],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def stop(self) -> None:
        """Close the pipe and wait for the writer to hand the LEDs back."""
        if not self.running:
            return
        writer, self.writer = self.writer, None
        try:
            writer.stdin.close()
        except OSError:
            pass
        try:
            writer.wait(timeout=5)
        except subprocess.TimeoutExpired:
            writer.kill()

    def step(self) -> None:
        """One frame: advance the phase and send the four colours."""
        if not self.running:
            return
        self.phase = (self.phase + FRAME / PERIODS[self.period]) % 1.0
        level = duty(shape(CURVES[self.curve], self.phase) * PEAKS[self.peak] / 100)
        lit = tuple(round(channel * level) for channel in COLOURS[self.colour][1])
        dark = (0, 0, 0)
        line = " ".join(str(v) for i in range(len(LEDS)) for v in (lit if self.on[i] else dark))
        try:
            self.writer.stdin.write((line + "\n").encode())
            self.writer.stdin.flush()
        except (OSError, ValueError):
            # The writer is gone, which means sudo refused or it was killed. Stop
            # rather than raise a broken pipe on every frame from here on.
            self.writer = None

    def rows(self) -> list[dict]:
        values = [
            (COLOURS[self.colour][0], self.colour, len(COLOURS)),
            (f"{PERIODS[self.period]:.1f}s", self.period, len(PERIODS)),
            (f"{PEAKS[self.peak]}%", self.peak, len(PEAKS)),
            (CURVES[self.curve], self.curve, len(CURVES)),
        ]
        rows = [
            {
                "label": label,
                "value": "ON" if self.on[i] else "OFF",
                "chevrons": 1,
                "at_start": False,
                "at_end": False,
            }
            for i, (_, label) in enumerate(LEDS)
        ]
        for label, (text, at, total) in zip(("Colour", "Period", "Peak", "Curve"), values):
            rows.append(
                {
                    "label": label,
                    "value": text,
                    "chevrons": 2,
                    "at_start": at == 0,
                    "at_end": at == total - 1,
                }
            )
        return rows

    def adjust(self, row: int, by: int) -> None:
        """Left or right on one row. A toggle flips; a value steps and stops at its ends."""
        if row < len(LEDS):
            self.on[row] = not self.on[row]
        elif row == COLOUR_ROW:
            self.colour = min(max(self.colour + by, 0), len(COLOURS) - 1)
        elif row == PERIOD_ROW:
            self.period = min(max(self.period + by, 0), len(PERIODS) - 1)
        elif row == PEAK_ROW:
            self.peak = min(max(self.peak + by, 0), len(PEAKS) - 1)
        elif row == CURVE_ROW:
            self.curve = min(max(self.curve + by, 0), len(CURVES) - 1)


def main() -> None:
    ui = flipctl.load(PAGE, "breathing-led.slint")
    breath = Breath()
    visible = int(flipctl.theme("list_visible_rows", 5))
    state = {"selected": 0, "scroll": 0, "arrow": 0}

    def draw() -> None:
        # The selected row is kept in the viewport, which is the whole of the
        # scrolling: MenuBody draws the window and its own scrollbar from these two.
        top = min(max(state["scroll"], state["selected"] - visible + 1), state["selected"])
        state["scroll"] = min(max(top, 0), max(0, ROWS - visible))
        ui.rows = breath.rows()
        ui.selected = state["selected"]
        ui.scroll = state["scroll"]
        ui.arrow_pressed = state["arrow"]
        ui.buttons = ["Close", "All", "", "None", "Stop" if breath.running else "Start"]

    async def leave() -> None:
        breath.stop()
        slint.quit_event_loop()

    @flipctl.on_key(ui)
    def _(key, down):
        # The chevron flash is the key's own press: held while the key is down and
        # gone when it comes up, which costs no timer.
        if key in (flipctl.Key.LEFT, flipctl.Key.RIGHT):
            state["arrow"] = 0 if not down else 1 if key is flipctl.Key.LEFT else 2
        if not down:
            draw()
            return
        if key in (flipctl.Key.BACK, flipctl.Key.ESCAPE):
            asyncio.get_running_loop().create_task(leave())
        elif key is flipctl.Key.DOWN:
            state["selected"] = (state["selected"] + 1) % ROWS
        elif key is flipctl.Key.UP:
            state["selected"] = (state["selected"] - 1) % ROWS
        elif key in (flipctl.Key.LEFT, flipctl.Key.RIGHT):
            breath.adjust(state["selected"], 1 if key is flipctl.Key.RIGHT else -1)
        elif key is flipctl.Key.OK and state["selected"] < len(LEDS):
            breath.adjust(state["selected"], 1)
        elif key is flipctl.Key.VIEW:
            breath.on = [True] * len(LEDS)
        elif key is flipctl.Key.EDIT:
            breath.on = [False] * len(LEDS)
        elif key is flipctl.Key.RUN:
            if breath.running:
                breath.stop()
            else:
                breath.start()
        draw()

    async def tick() -> None:
        while True:
            await asyncio.sleep(FRAME)
            breath.step()

    draw()
    flipctl.run(ui, tick())


if __name__ == "__main__":
    main()
