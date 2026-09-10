#!/usr/bin/env python3
# /// flipctl
# name = "Network LEDs"
# status = true
# ///
"""The four RGB LEDs along the top of the device, as a test tool.

They normally follow the network ports: the kernel's netdev trigger lights each one
from its interface's carrier, set up by 99-network-leds.rules in the image. This
takes them off that trigger, walks each colour channel so a dead diode or a dead
channel is visible, and hands them back when it leaves.

Nothing here describes a screen: the rows and the frame are flipctl's own widgets,
and the panel's design tokens are read from the theme the runtime carries rather
than copied.

Up and down choose an LED, Test walks the selected one through red, green, blue and
white, All does the four together, Restore gives them back to the kernel, and Back
restores and leaves.
"""

import asyncio
import pathlib
import subprocess

import flipctl
import slint

LEDS = pathlib.Path("/sys/class/leds")
# Name on the device, and what to call it on screen, in the order they sit along
# the top of the device.
ROWS = [("eth0", "ETH 0"), ("eth1", "ETH 1"), ("wifi", "Wi-Fi"), ("link", "Link")]
# One step per channel, then all three, so a diode that is dark on one colour shows
# it. The value is what multi_intensity takes: red, green and blue in that order.
SEQUENCE = [("red", "255 0 0"), ("green", "0 255 0"), ("blue", "0 0 255"), ("white", "255 255 255")]
STEP = 0.6

# A row of our own, carrying only what this screen has to say. The list body takes
# ListItem, which has fields for icons and adjustable values that a Python caller
# has no way to fill: an image cannot be left out of a struct handed over from
# Python, and there is nothing sensible to put in it. Four LEDs is a fixed number,
# so the rows are turned into ListItems here, where a struct literal fills
# everything it does not mention with defaults.
PAGE = """
import { Shell } from "@app/shell.slint";
import { MenuBody } from "@flipctl/list.slint";

export struct LedRow {
    label: string,
    status: string,
}

export component App inherits Shell {
    in property <[LedRow]> rows;
    in property <int> selected;
    in property <[string]> buttons;
    callback keyed(string, bool);
    key(text, down) => { root.keyed(text, down); }

    MenuBody {
        items: [
            { label: root.rows[0].label, status: root.rows[0].status },
            { label: root.rows[1].label, status: root.rows[1].status },
            { label: root.rows[2].label, status: root.rows[2].status },
            { label: root.rows[3].label, status: root.rows[3].status },
        ];
        selected: root.selected;
        total: 4;
        real_frame: true;
        buttons: root.buttons;
    }
}
"""


def path(led: str) -> pathlib.Path:
    return LEDS / f"flipper-one:rgb:{led}"


def read(led: str, name: str) -> str:
    try:
        return (path(led) / name).read_text().strip()
    except OSError:
        return ""


def trigger_of(led: str) -> str:
    """The active trigger, which sysfs marks with brackets among all the others."""
    text = read(led, "trigger")
    start, end = text.find("["), text.find("]")
    return text[start + 1 : end] if 0 <= start < end else "?"


def run_as_root(script: str) -> None:
    """Write to sysfs, which is root's. Blocking, so callers hand it to a thread.

    The whole write goes in one shell rather than one process per file: an LED costs
    three writes and a sequence step should not cost three sudo invocations.
    """
    subprocess.run(
        ["sudo", "sh", "-c", script],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def paint_script(led: str, colour: str, brightness: int) -> str:
    p = path(led)
    return (
        f"printf none > {p}/trigger; "
        f"printf '%s' '{colour}' > {p}/multi_intensity; "
        f"printf {brightness} > {p}/brightness"
    )


async def paint(leds: list[str], colour: str, brightness: int = 255) -> None:
    await asyncio.to_thread(run_as_root, "; ".join(paint_script(n, colour, brightness) for n in leds))


async def restore() -> None:
    """Give them back to the kernel, by replaying the event the rules hang off.

    Re-running the udev rules is the whole restore: they load the trigger, bind each
    LED to its interface and set the colours, so nothing here has to know what the
    right colours were.
    """
    await asyncio.to_thread(
        run_as_root, "udevadm trigger --subsystem-match=net --action=add; udevadm settle"
    )


def status(led: str) -> str:
    """What the LED is doing, in the few characters a row's right edge allows."""
    trigger = trigger_of(led)
    lit = read(led, "brightness") not in ("", "0")
    if trigger == "netdev":
        return f"{read(led, 'device_name') or 'auto'} {'on' if lit else 'off'}"
    if not lit:
        return "off"
    return read(led, "multi_intensity")


def main() -> None:
    ui = flipctl.load(PAGE, "network-leds.slint")
    state = {"selected": 0, "note": ""}

    def draw() -> None:
        ui.rows = [
            {
                "label": label,
                "status": state["note"] if state["note"] and i == state["selected"] else status(led),
            }
            for i, (led, label) in enumerate(ROWS)
        ]
        ui.selected = state["selected"]
        ui.buttons = ["Close", "All", "", "Restore", "Test"]

    async def sweep(leds: list[str]) -> None:
        """Walk the channels, leaving the LEDs back on the trigger afterwards."""
        for name, colour in SEQUENCE:
            state["note"] = name
            draw()
            await paint(leds, colour)
            await asyncio.sleep(STEP)
        state["note"] = ""
        await paint(leds, "0 0 0", 0)
        await restore()
        draw()

    work: dict[str, asyncio.Task | None] = {"task": None}

    def start(leds: list[str]) -> None:
        if work["task"] and not work["task"].done():
            return
        work["task"] = asyncio.get_running_loop().create_task(sweep(leds))

    @flipctl.on_key(ui)
    def _(key, down):
        if not down:
            return
        if key in (flipctl.Key.BACK, flipctl.Key.ESCAPE):
            asyncio.get_running_loop().create_task(leave())
        elif key is flipctl.Key.DOWN:
            state["selected"] = (state["selected"] + 1) % len(ROWS)
        elif key is flipctl.Key.UP:
            state["selected"] = (state["selected"] - 1) % len(ROWS)
        elif key is flipctl.Key.RUN:
            start([ROWS[state["selected"]][0]])
        elif key is flipctl.Key.VIEW:
            start([led for led, _ in ROWS])
        elif key is flipctl.Key.EDIT:
            asyncio.get_running_loop().create_task(restore_and_draw())
        draw()

    async def restore_and_draw() -> None:
        await restore()
        draw()

    async def leave() -> None:
        await restore()
        slint.quit_event_loop()

    async def tick() -> None:
        """The rows show live sysfs, so the screen follows a cable being plugged."""
        while True:
            await asyncio.sleep(1)
            draw()

    draw()
    flipctl.run(ui, tick())


if __name__ == "__main__":
    main()
