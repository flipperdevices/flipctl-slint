#!/usr/bin/env python3
# /// flipctl
# name = "NAU8822 Bring-up"
# status = true
# icon = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAA4AAAAOCAYAAAAfSC3RAAAASklEQVR42rWSwQoAIAhDt9H//3JdCjw0qbDdVOZjKGDUp9xcqFQkOarKaTHjjqov2bKsKqW5O8a6uUUkeU3LPmf1VJbt1MMXIwAMSIFbuKsnDG8AAAAASUVORK5CYII="
# ///
"""Bring up the on-board NAU8822 codec when it did not come up at boot.

On some boots the codec's driver fails its reset write at probe ("Failed to issue
reset: -6": the chip did not acknowledge on I2C bus 6, address 0x1a), and the
speaker and the mic are missing. The chip is not dead: the first access to it after
it has sat idle can be refused, and the next one goes through.

The page shows whether ALSA has the NAU8822 card. Run, when it has not:

1. reads the chip's ID register once, `i2ctransfer -y 6 w1@0x1a 0x7e r2`, and
   ignores the answer: it is only there to be the first access;
2. writes `6-001a` to /sys/bus/i2c/drivers/nau8822/bind, which probes the driver
   again, and waits a second for the card;
3. repeats both, up to three times, until /proc/asound/cards lists the NAU8822.

Both steps need root and go through `sudo -n`, as flipctl's own claims do. Each
step's outcome is listed under the state. If three tries do not bring it up, only a
power cycle will.
"""

import pathlib
import subprocess
import time

import flipctl

CARDS = pathlib.Path("/proc/asound/cards")
DRIVER = pathlib.Path("/sys/bus/i2c/drivers/nau8822")
CHIP = "6-001a"
TRIES = 3

said: list[str] = []


def present() -> bool:
    try:
        return "NAU8822" in CARDS.read_text()
    except OSError:
        return False


def run(*cmd: str) -> bool:
    """One step through sudo, its outcome added to the list."""
    done = subprocess.run(["sudo", "-n", *cmd], capture_output=True, text=True)
    out = (done.stdout + done.stderr).strip().splitlines()
    said.append(f"{' '.join(cmd[:2])}: {'ok' if done.returncode == 0 else (out[-1] if out else 'failed')}")
    return done.returncode == 0


def bring_up() -> None:
    said.clear()
    if present():
        said.append("already up")
        return
    for attempt in range(1, TRIES + 1):
        if present():
            break
        # The throwaway access: refused or not, the next one is what counts
        run("i2ctransfer", "-y", "6", "w1@0x1a", "0x7e", "r2")
        subprocess.run(
            ["sudo", "-n", "tee", str(DRIVER / "bind")],
            input=CHIP, capture_output=True, text=True,
        )
        time.sleep(1)
        said.append(f"bind {attempt}: {'up' if present() else 'not yet'}")
    said.append("NAU8822 is up" if present() else "still missing: power the Flipper off and on")


def rows() -> list[dict]:
    out = [flipctl.pair("NAU8822", "up" if present() else "missing"), flipctl.rule()]
    out += [flipctl.text(line) for line in said]
    return out


def main() -> None:
    app = flipctl.Detail(rows=rows, buttons=["Close", "", "", "", "Run"])

    @app.on_key
    def _(key):
        if key is flipctl.Key.RUN:
            bring_up()
            return True
        return False

    app.run(every=1)


if __name__ == "__main__":
    main()
