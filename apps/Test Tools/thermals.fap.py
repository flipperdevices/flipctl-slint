#!/usr/bin/env python3
# /// flipctl
# name = "Thermals"
# status = true
# ///
"""Every temperature sensor the kernel exposes, refreshed once a second.

The SoC carries one sensor per block rather than a single chip-wide figure, and
they do not agree: the one near the centre of the die reads several degrees below
either core cluster, so a tool reading one zone and a tool reading another
disagree about "the CPU temperature". This shows all of them at once, which is the
only way to see which one a given reading came from.

Thermal zones are the kernel's own list. hwmon is read as well and anything it
carries that has no zone is added, which is how the Wi-Fi chip's sensor appears:
it is a hwmon device and nothing registers it as a zone.

The names are shortened to what the panel's left column holds, so the whole set
fits on one screen with nothing to scroll.
"""

import asyncio
import pathlib

import flipctl
import slint

THERMAL = pathlib.Path("/sys/class/thermal")
HWMON = pathlib.Path("/sys/class/hwmon")

# What to call each sensor. The kernel's own names are too long for the column and
# say the same thing twice: every zone ends in -thermal. The clusters keep ARM's own
# big.LITTLE terms, which describe the cores rather than the use they are put to.
NAMES = {
    "package-thermal": "SoC",
    "bigcore-thermal": "Big cores",
    "littlecore-thermal": "Little cores",
    "gpu-thermal": "GPU",
    "npu-thermal": "NPU",
    "ddr-thermal": "DDR",
    "bq28z610-0": "Battery",
    "mt7921_phy0": "Wi-Fi",
}

PAGE = """
import { Shell } from "@app/shell.slint";
import { DetailBody, DetailRow } from "@flipctl/detail.slint";

export { DetailRow }

export component App inherits Shell {
    in property <[DetailRow]> rows;
    in property <int> offset;
    in property <[string]> buttons;
    callback keyed(string, bool);
    key(text, down) => { root.keyed(text, down); }

    DetailBody {
        rows: root.rows;
        breadcrumb: "Thermals";
        offset: root.offset;
        buttons: root.buttons;
    }
}
"""

# Rows the body shows at once. The sensors fit inside this today; the offset is
# kept so a kernel that adds one does not hide it.
VISIBLE = int(flipctl.theme("detail_visible_rows_bare") or flipctl.theme("detail_visible_rows", 8))


def read(path: pathlib.Path) -> str:
    try:
        return path.read_text().strip()
    except OSError:
        return ""


def milli(path: pathlib.Path) -> int | None:
    text = read(path)
    try:
        return int(text)
    except ValueError:
        return None


def celsius(value: int) -> str:
    """Tenths, which is what the sensors actually resolve to."""
    return f"{value / 1000:.1f}C"


def label(name: str) -> str:
    """Anything not named above keeps its own name without the shared suffix."""
    return NAMES.get(name, name.removesuffix("-thermal"))


def index(path: pathlib.Path, prefix: str) -> int:
    """Sort key, so zones stay in the kernel's order rather than readdir's."""
    try:
        return int(path.name[len(prefix) :])
    except ValueError:
        return 0


def zones() -> list[tuple[str, int]]:
    out = []
    if not THERMAL.is_dir():
        return out
    for zone in sorted(THERMAL.glob("thermal_zone*"), key=lambda p: index(p, "thermal_zone")):
        name = read(zone / "type")
        value = milli(zone / "temp")
        if name and value is not None:
            out.append((name, value))
    return out


def hwmon_only(known: set[str]) -> list[tuple[str, int]]:
    """hwmon sensors that no thermal zone already covers.

    A zone's type spells itself with hyphens and its hwmon device with
    underscores, so the chip is matched with that flattened. The whole chip is
    skipped rather than each reading: the fuel gauge registers a zone and a hwmon
    whose only label is "temp", and matching on the label would show the pack
    twice under a name that says nothing.
    """
    out = []
    if not HWMON.is_dir():
        return out
    for chip in sorted(HWMON.glob("hwmon*"), key=lambda p: index(p, "hwmon")):
        chip_name = read(chip / "name")
        if not chip_name or chip_name.replace("_", "-") in known:
            continue
        inputs = sorted(chip.glob("temp*_input"))
        for path in inputs:
            value = milli(path)
            if value is None:
                continue
            # One sensor is just the chip. Several have to be told apart, and a
            # label only helps when it says more than "temp".
            name = chip_name
            if len(inputs) > 1:
                extra = read(pathlib.Path(str(path)[: -len("_input")] + "_label"))
                name = f"{chip_name} {extra or path.name.split('_')[0]}"
            out.append((name, value))
    return out


def sensors() -> list[tuple[str, int]]:
    found = zones()
    known = {name.replace("_", "-") for name, _ in found}
    return found + hwmon_only(known)


def rows_for(readings: list[tuple[str, int]]) -> list[dict]:
    """Rows for the body, every field of the struct spelled out.

    A struct handed over from Python has to carry all of its fields: a row that
    leaves `dim` out reaches the body as nothing rather than false, and the
    conditional that reads it takes the interpreter down with it.
    """
    return [
        {"kind": 0, "label": label(name), "value": celsius(value), "percent": 0, "dim": False}
        for name, value in readings
    ]


def main() -> None:
    ui = flipctl.load(PAGE, "thermals.slint")
    state = {"offset": 0, "rows": []}

    def draw() -> None:
        state["rows"] = rows_for(sensors())
        # A sensor can appear when a driver loads, so the offset is clamped to
        # what is there now rather than to what was there when a key was pressed.
        limit = max(0, len(state["rows"]) - VISIBLE)
        state["offset"] = min(state["offset"], limit)
        ui.rows = state["rows"]
        ui.offset = state["offset"]
        ui.buttons = ["Close", "", "", "", ""]

    @flipctl.on_key(ui)
    def _(key, down):
        if not down:
            return
        if key in (flipctl.Key.BACK, flipctl.Key.ESCAPE):
            slint.quit_event_loop()
            return
        limit = max(0, len(state["rows"]) - VISIBLE)
        if key is flipctl.Key.DOWN:
            state["offset"] = min(state["offset"] + 1, limit)
        elif key is flipctl.Key.UP:
            state["offset"] = max(state["offset"] - 1, 0)
        draw()

    async def tick() -> None:
        while True:
            await asyncio.sleep(1)
            draw()

    draw()
    flipctl.run(ui, tick())


if __name__ == "__main__":
    main()
