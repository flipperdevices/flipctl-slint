#!/usr/bin/env python3
# /// flipctl
# name = "ETH LED"
# status = true
# ///
"""The eth0 LED saying what the port is actually doing, in three states.

  no cable      dark, faded out over a second rather than switched off
  cable, no IP  faded in, then breathing between a floor and full: the port is up
                and still negotiating
  IP            solid

Which is more than the netdev trigger can say. It has one bit, the carrier, so a
cable into a switch that never answers DHCP looks exactly like a working port. The
middle state is the whole point of this: breathing means "plugged in, no address
yet", and it stops the moment there is one.

Every transition is a fade, and each fade's length is on the screen and adjustable,
because how long they should be is a matter of taste and the only way to settle it
is to watch it. A fade runs from wherever the light happens to be, so unplugging
half way through an inhale slides down from there rather than jumping.

The fades and the breath are drawn in perceived lightness and converted to duty
with CIE 1931, and the breath is the asymmetric gaussian the Breathing LED app
explains at length. The short version: the eye's response to light is close to a
cube root, so a linear ramp of duty is not a linear ramp to look at.

Only eth0 is touched. It goes back on the kernel's netdev trigger when the app
leaves, or when it is killed, since the root writer that took it also returns it.
"""

import asyncio
import fcntl
import math
import pathlib
import socket
import struct
import subprocess

import flipctl
import slint

# The interface behind the LED called eth0, which is what 99-network-leds.rules
# binds it to.
PORT = "end0"
CARRIER = pathlib.Path(f"/sys/class/net/{PORT}/carrier")
# The colour the image gives the ethernet ports.
COLOUR = (120, 255, 0)
FRAME = 0.02
SIOCGIFADDR = 0x8915

# Where the breath peaks and how wide it is on each side, as fractions of the cycle:
# narrow rising, wide falling, and the falling side runs out before the cycle does,
# which leaves the rest of it dark. That is the pause between breaths.
MU, SIGMA_IN, SIGMA_OUT = 0.30, 0.10, 0.17

# The adjustable settings, each a list of the values the row steps through and the
# unit it reads in. Floor is where the breath bottoms out instead of going dark: a
# breath that reaches zero reads as the port dropping out and coming back, and a
# port that is up should not look like one that is not.
#
# It defaults to 35% because that is the first step the hardware keeps. The floor is
# in perceived lightness, so 35% is only 8.5% duty, and the LED is RGB565: below that
# the lime's red channel rounds to zero and the bottom of the breath turns plain
# green, and by 10% both channels round away and it is off after all. Measured, not
# guessed. Lower steps are still there for a darker port that wants them.
# From zero, because an instant change is a setting too: the way to find out
# whether a fade is worth having is to turn it off and look.
TENTHS = [round(0.2 * i, 1) for i in range(0, 26)]
PERIODS = [round(1.0 + 0.5 * i, 1) for i in range(15)]
FLOORS = list(range(0, 61, 5))
SETTINGS = [
    ("Fade out", "fade_out", TENTHS, TENTHS.index(1.0), "s"),
    ("Fade in", "fade_in", TENTHS, TENTHS.index(1.0), "s"),
    ("Breathe", "period", PERIODS, PERIODS.index(2.5), "s"),
    ("Solid", "solid", TENTHS, TENTHS.index(0.6), "s"),
    ("Floor", "floor", FLOORS, FLOORS.index(35), "%"),
]
FACTS = ("Link", "Address", "LED")
ROWS = len(FACTS) + len(SETTINGS)

# Slint has no way to build one array out of another, and a Python caller cannot
# fill ListItem's `image` field, so each row is turned into a ListItem by a literal,
# which fills every field it does not mention with a default. Written out here
# rather than by hand so the count follows the rows above.
ITEM = """            {{ label: root.rows[{i}].label, status: root.rows[{i}].status,
              value: root.rows[{i}].value, chevrons: root.rows[{i}].chevrons,
              dim: root.rows[{i}].dim, at_start: root.rows[{i}].at_start,
              at_end: root.rows[{i}].at_end }},"""

PAGE = """
import { Shell } from "@app/shell.slint";
import { MenuBody } from "@flipctl/list.slint";

export struct EthRow {
    label: string,
    status: string,
    value: string,
    chevrons: int,
    dim: bool,
    at_start: bool,
    at_end: bool,
}

export component App inherits Shell {
    in property <[EthRow]> rows;
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
# instead of one per frame, and hands the LED back on the way out: the pipe closing
# or a TERM both get there, and flipctl signals the whole group when an app is
# closed from the switcher. Replaying the udev event is the whole restore, since the
# rules load the netdev trigger, bind the LED to its interface and set the colour.
#
# A write that would change nothing is dropped. The LEDs share an i2c device with
# the buttons at 400kHz, and a fade that has reached its end has nothing to say.
WRITER = r"""
import os, signal, sys

LED = "/sys/class/leds/flipper-one:rgb:eth0/"
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
with open(LED + "trigger", "w") as f:
    f.write("none")
with open(LED + "brightness", "w") as f:
    f.write("255")
fd = os.open(LED + "multi_intensity", os.O_WRONLY)
held = None
try:
    while True:
        line = sys.stdin.readline()
        if not line:
            break
        colour = " ".join(line.split())
        if len(colour.split()) != 3:
            break
        if colour != held:
            held = colour
            os.pwrite(fd, colour.encode(), 0)
finally:
    os.system("udevadm trigger --subsystem-match=net --action=add; udevadm settle")
"""


def breath(phase: float) -> float:
    """Perceived lightness, 0 to 1, at `phase` through one breath.

    Normalised per side so both ends of the cycle reach exactly zero: the sides have
    different widths, so their tails are different heights and subtracting a single
    floor would leave a step at the wrap.
    """
    sigma = SIGMA_IN if phase < MU else SIGMA_OUT
    edge = (MU if phase < MU else 1 - MU) / sigma
    floor = math.exp(-edge * edge / 2)
    x = (phase - MU) / sigma
    return (math.exp(-x * x / 2) - floor) / (1 - floor)


def duty(lightness: float) -> float:
    """The luminance a perceived lightness needs, by CIE 1931. `lightness` is 0..1."""
    star = 100 * lightness
    return star / 903.3 if star <= 8 else ((star + 16) / 116) ** 3


def carrier() -> bool:
    """Whether there is a cable. Unreadable while the interface is down, which is no."""
    try:
        return CARRIER.read_text().strip() == "1"
    except OSError:
        return False


def address() -> str:
    """The port's IPv4 address, or empty.

    A 169.254 one does not count: that is what the kernel gives itself when nothing
    answered, so it means the opposite of having an address, and the LED would stop
    breathing exactly when it should not.
    """
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        try:
            packed = fcntl.ioctl(
                sock.fileno(), SIOCGIFADDR, struct.pack("256s", PORT.encode()[:15])
            )
        except OSError:
            return ""
    found = socket.inet_ntoa(packed[20:24])
    return "" if found.startswith("169.254.") else found


class Lamp:
    """The light, as a level in perceived lightness and the state driving it.

    A fade is held as where it started and how far through it is, rather than as a
    curve of its own, so it can begin anywhere: the level at the moment the cable
    moved is where the next fade starts from.
    """

    def __init__(self) -> None:
        self.state = "dark"
        self.level = 0.0
        self.phase = MU
        self.came_from = 0.0
        self.through = 1.0

    def enter(self, state: str) -> None:
        self.came_from = self.level
        self.through = 0.0
        self.state = state
        # The fade in ends at the top of the breath, so the breath carries on
        # downward from there into its first exhale rather than restarting.
        if state == "link":
            self.phase = MU

    @property
    def target(self) -> float:
        return 0.0 if self.state == "dark" else 1.0

    @property
    def fading(self) -> bool:
        return self.through < 1.0

    def step(self, fade: float, period: float, floor: float = 0.0) -> None:
        if self.fading:
            self.through = 1.0 if fade <= 0 else min(1.0, self.through + FRAME / fade)
            self.level = self.came_from + (self.target - self.came_from) * self.through
        elif self.state == "link":
            self.phase = (self.phase + FRAME / period) % 1.0
            # The floor lifts the whole breath rather than clipping its bottom, so
            # the shape is the same one, drawn between the floor and full.
            self.level = floor + (1 - floor) * breath(self.phase)
        else:
            self.level = self.target

    def says(self) -> str:
        if self.fading:
            return "fading"
        return {"dark": "dark", "link": "breathing", "ip": "solid"}[self.state]


class Port:
    """The app: what the port is doing, what the light is doing, and the pipe between."""

    def __init__(self) -> None:
        self.lamp = Lamp()
        self.link = False
        self.ip = ""
        self.at = {name: start for _, name, _, start, _ in SETTINGS}
        self.writer: subprocess.Popen | None = None

    def value(self, name: str) -> float:
        choices = next(c for _, n, c, _, _ in SETTINGS if n == name)
        return choices[self.at[name]]

    def start(self) -> None:
        self.writer = subprocess.Popen(
            ["sudo", "python3", "-c", WRITER],
            stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def stop(self) -> None:
        if self.writer is None:
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

    def look(self) -> None:
        """Read the port and move the lamp to the state it calls for."""
        self.link, self.ip = carrier(), ""
        if self.link:
            self.ip = address()
        wants = "ip" if self.ip else "link" if self.link else "dark"
        if wants != self.lamp.state:
            self.lamp.enter(wants)

    def step(self) -> None:
        fade = self.value({"dark": "fade_out", "link": "fade_in", "ip": "solid"}[self.lamp.state])
        self.lamp.step(fade, self.value("period"), self.value("floor") / 100)
        if self.writer is None:
            return
        level = duty(self.lamp.level)
        line = " ".join(str(round(channel * level)) for channel in COLOUR)
        try:
            self.writer.stdin.write((line + "\n").encode())
            self.writer.stdin.flush()
        except (OSError, ValueError):
            # The writer is gone, which means sudo refused or it was killed. Stop
            # rather than raise a broken pipe on every frame from here on.
            self.writer = None

    def rows(self) -> list[dict]:
        facts = [
            ("Link", "up" if self.link else "down"),
            ("Address", self.ip or "-"),
            ("LED", self.lamp.says()),
        ]
        rows = [
            {
                "label": label,
                "status": status,
                "value": "",
                "chevrons": 0,
                "dim": True,
                "at_start": False,
                "at_end": False,
            }
            for label, status in facts
        ]
        for label, name, choices, _, unit in SETTINGS:
            shown = choices[self.at[name]]
            rows.append(
                {
                    "label": label,
                    "status": "",
                    "value": f"{shown:.1f}s" if unit == "s" else f"{shown}{unit}",
                    "chevrons": 2,
                    "dim": False,
                    "at_start": self.at[name] == 0,
                    "at_end": self.at[name] == len(choices) - 1,
                }
            )
        return rows

    def adjust(self, row: int, by: int) -> None:
        if row < len(FACTS):
            return
        _, name, choices, _, _ = SETTINGS[row - len(FACTS)]
        self.at[name] = min(max(self.at[name] + by, 0), len(choices) - 1)


def main() -> None:
    ui = flipctl.load(PAGE, "eth-led.slint")
    port = Port()
    visible = int(flipctl.theme("list_visible_rows", 5))
    # Opened on the first timing rather than the first row: the three above it are
    # readings, and left and right do nothing on them.
    state = {"selected": len(FACTS), "scroll": 0, "arrow": 0}

    def draw() -> None:
        top = min(max(state["scroll"], state["selected"] - visible + 1), state["selected"])
        state["scroll"] = min(max(top, 0), max(0, ROWS - visible))
        ui.rows = port.rows()
        ui.selected = state["selected"]
        ui.scroll = state["scroll"]
        ui.arrow_pressed = state["arrow"]
        ui.buttons = ["Close", "", "", "", ""]

    async def leave() -> None:
        port.stop()
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
            port.adjust(state["selected"], 1 if key is flipctl.Key.RIGHT else -1)
        draw()

    async def tick() -> None:
        """The light every frame, the port five times a second, the screen once.

        Reading sysfs and asking the kernel for an address are both cheap, but not so
        cheap that fifty times a second is worth it for a cable nobody is touching,
        and a screen that repaints at the frame rate would keep the panel busy for
        four rows that change once a minute.
        """
        counted = 0
        while True:
            await asyncio.sleep(FRAME)
            counted += 1
            said = port.lamp.says()
            if counted % 10 == 0:
                port.look()
            port.step()
            if counted % 50 == 0 or port.lamp.says() != said:
                draw()

    port.look()
    port.start()
    draw()
    flipctl.run(ui, tick())


if __name__ == "__main__":
    main()
