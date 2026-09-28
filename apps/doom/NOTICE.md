# Third-party material in this bundle

This bundle installs nothing and therefore carries nearly everything it runs. Almost
all of it is somebody else's work, redistributed under the terms below.

## The engine

`runtime/usr/games/dsda-doom` is [dsda-doom](https://github.com/kraflab/dsda-doom),
released under the **GNU General Public License version 2**.

## The game data

`runtime/usr/share/games/doom/freedoom1.wad` is
[Freedoom](https://freedoom.github.io/) Phase 1, and its textures, sprites, sounds
and status bar are licensed **BSD 3-Clause**:

    Copyright 2001-2024 Contributors to the Freedoom project

Two of its lumps are not. Freedoom licenses `PLAYPAL` and `COLORMAP` under the **GNU
General Public License version 2 or later**, separately from the rest of the project:

    Copyright 2001 Colin Phipps <cphipps@doomworld.com>
    Copyright 2008, 2013 Simon Howard
    Copyright 1999 id Software (http://www.idsoftware.com/)

## The palette

`palette.wad` is ours to the extent that a lookup table can be: it is Freedoom's
`PLAYPAL`, every colour replaced by the grey the Flipper One's panel renders it as,
under the tone curve chosen on the panel itself. Being a derivative of that lump, it
carries that lump's terms, GPL-2.0-or-later.

## The libraries

`runtime/` also holds seventy-seven other Debian packages, from SDL and the engine's
music backends down to zlib, because the app is not allowed to install any of them.
Each one keeps its own terms, and every package's Debian copyright file travels with
it at `runtime/usr/share/doc/<package>/copyright`.

Several are under the GPL or LGPL, which oblige us to say where the corresponding
source is: `deb.lock` in this app's source directory names every package by exact
version with the archive URL it was fetched from, and Debian publishes the source for
each of those at the same archive. Nothing in `runtime/` was modified; the packages
are unpacked as published and pruned only of documentation, manual pages and
translations, never of code.

The licence texts referred to here are in `LICENSES/`.
