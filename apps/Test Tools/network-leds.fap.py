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
white, All does the four together, Restore gives them back to the kernel, and Close
restores and leaves.
"""

import asyncio
import pathlib
import subprocess

import flipctl

LEDS = pathlib.Path("/sys/class/leds")
# Name on the device, and what to call it on screen, in the order they sit along
# the top of the device.
ROWS = [("eth0", "ETH 0"), ("eth1", "ETH 1"), ("wifi", "Wi-Fi"), ("link", "Link")]
# One step per channel, then all three, so a diode that is dark on one colour shows
# it. The value is what multi_intensity takes: red, green and blue in that order.
SEQUENCE = [("red", "255 0 0"), ("green", "0 255 0"), ("blue", "0 0 255"), ("white", "255 255 255")]
STEP = 0.6


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
    note = {"text": ""}
    work: dict[str, asyncio.Task | None] = {"task": None}

    def items():
        return [
            flipctl.item(label, status=note["text"] if note["text"] and i == app.selected else status(led))
            for i, (led, label) in enumerate(ROWS)
        ]

    app = flipctl.Menu(items=items, buttons=["Close", "All", "", "Restore", "Test"], on_close=restore)

    async def sweep(leds: list[str]) -> None:
        """Walk the channels, leaving the LEDs back on the trigger afterwards."""
        for name, colour in SEQUENCE:
            note["text"] = name
            app.redraw()
            await paint(leds, colour)
            await asyncio.sleep(STEP)
        note["text"] = ""
        await paint(leds, "0 0 0", 0)
        await restore()
        app.redraw()

    def start(leds: list[str]) -> None:
        if work["task"] and not work["task"].done():
            return
        work["task"] = asyncio.get_running_loop().create_task(sweep(leds))

    async def restore_and_draw() -> None:
        await restore()
        app.redraw()

    @app.on_key
    def _(key):
        if key is flipctl.Key.RUN:
            start([ROWS[app.selected][0]])
        elif key is flipctl.Key.VIEW:
            start([led for led, _ in ROWS])
        elif key is flipctl.Key.EDIT:
            asyncio.get_running_loop().create_task(restore_and_draw())
        else:
            return False
        return True

    # The rows show live sysfs, so the screen follows a cable being plugged.
    app.run(every=1)


if __name__ == "__main__":
    main()
