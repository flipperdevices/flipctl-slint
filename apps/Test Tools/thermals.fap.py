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

import pathlib

import flipctl

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


def rows() -> list[dict]:
    return [flipctl.pair(label(name), celsius(value)) for name, value in sensors()]


def main() -> None:
    # A sensor can appear when a driver loads, so the rows are read again each second
    # rather than once.
    flipctl.Detail(rows=rows, buttons=["Close", "", "", "", ""]).run(every=1)


if __name__ == "__main__":
    main()
