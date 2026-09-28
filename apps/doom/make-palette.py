"""Bake the tone curve Tone Lab settled on into a Doom palette.

The panel is black ink on an orange ground, and the engine's stock palette is
made for a backlit screen. The correction is one output grey per input
grey, which is exactly what a palette can express, so the curve chosen by looking
at Tone Lab reproduces here without approximation.

Each entry becomes a neutral grey: the panel is greyscale and converts colour to
Rec.601 luma on the way out, so taking that luma here and writing it back as R=G=B
gives the panel the same pixel it would have computed, and nothing is lost.

All fourteen pages are mapped, not just the base one. The others are the damage and
pickup tints, and a page left uncorrected would flash at a different brightness
from the game under it.

    python3 apps/doom/make-palette.py freedoom1.wad apps/doom/palette.wad
"""

import math
import struct
import sys

# The numbers Tone Lab was left on, in its own terms: gamma, output floor, output
# ceiling, and the S-curve amount.
GAMMA = 0.50
FLOOR = 0.0
CEILING = 255.0
SHAPE = 1.0

PAGES = 14
ENTRIES = 256
PAGE_BYTES = ENTRIES * 3


def lump(path, want):
    with open(path, "rb") as f:
        _, count, directory = struct.unpack("<4sii", f.read(12))
        f.seek(directory)
        entries = f.read(count * 16)
        for i in range(count):
            pos, size, raw = struct.unpack_from("<ii8s", entries, i * 16)
            if raw.rstrip(b"\0").decode() == want:
                f.seek(pos)
                return f.read(size)
    raise SystemExit("no %s lump in %s" % (want, path))


def half_up(x):
    """Round half away from zero, which is what Rust's f32::round does.

    Tone Lab writes the same palette from the same numbers, and Python's own round
    goes to even on a tie, so without this the two would disagree by one grey on
    any entry that lands exactly on a half.
    """
    return math.floor(x + 0.5)


def shape(v, amount):
    """An S-curve either way: positive steepens the midtones, negative flattens.

    Smoothstep is the steepening half and its exact inverse is the flattening half,
    so the two directions undo each other.
    """
    if amount == 0.0:
        return v
    if amount > 0.0:
        target = v * v * (3.0 - 2.0 * v)
    else:
        target = 0.5 - math.sin(math.asin(max(-1.0, min(1.0, 1.0 - 2.0 * v))) / 3.0)
    return v + (target - v) * abs(amount)


def table():
    out = bytearray(256)
    for i in range(256):
        v = (i / 255.0) ** GAMMA
        v = shape(v, SHAPE)
        grey = FLOOR + v * (CEILING - FLOOR)
        out[i] = max(0, min(255, half_up(grey)))
    return out


def main():
    iwad, target = sys.argv[1], sys.argv[2]
    source = lump(iwad, "PLAYPAL")
    if len(source) < PAGES * PAGE_BYTES:
        raise SystemExit("PLAYPAL is %d bytes, short of %d" % (len(source), PAGES * PAGE_BYTES))

    lut = table()
    data = bytearray()
    for page in range(PAGES):
        base = page * PAGE_BYTES
        for i in range(base, base + PAGE_BYTES, 3):
            r, g, b = source[i], source[i + 1], source[i + 2]
            y = half_up(0.299 * r + 0.587 * g + 0.114 * b)
            data += bytes([lut[min(255, y)]]) * 3

    with open(target, "wb") as f:
        f.write(struct.pack("<4sii", b"PWAD", 1, 12 + len(data)))
        f.write(data)
        f.write(struct.pack("<ii8s", 12, len(data), b"PLAYPAL"))
    print("wrote %s: %d pages, gamma %.2f floor %g ceiling %g s-curve %.2f"
          % (target, PAGES, GAMMA, FLOOR, CEILING, SHAPE))
    print("black %d, mid %d, white %d" % (lut[0], lut[128], lut[255]))


main()
