# Third-party material in this bundle

Two things here come from [Freedoom](https://freedoom.github.io/).

`playpal.bin` is the project's `PLAYPAL` lump, verbatim: the palette the correction
is applied to, so that what Save writes is a correction of the engine's real
colours rather than of a guess at them. Freedoom licenses that lump under the **GNU
General Public License version 2 or later**, separately from the rest of the
project:

    Copyright 2001 Colin Phipps <cphipps@doomworld.com>
    Copyright 2008, 2013 Simon Howard
    Copyright 1999 id Software (http://www.idsoftware.com/)

`frames/1.rgb` to `frames/4.rgb` are four frames of Freedoom Phase 1, rendered
headless at panel size by dsda-doom and kept as raw pixels, in colour because the
channel weights are one of the things being chosen. They show the
project's own textures, sprites and status bar, which are licensed **BSD 3-Clause**:

    Copyright 2001-2024 Contributors to the Freedoom project

Both sets of terms are in `LICENSES/`. The binary as a whole ships under
GPL-3.0-only, as every app here does, which the palette's "or later" permits.
