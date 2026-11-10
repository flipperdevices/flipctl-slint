# Programs this bundle carries

The radio is BirriaSDR, unmodified, from its Flipper One release. Its license texts
travel beside it under `usr/share/doc/birriasdr/`.

| Program | Version | License | Source |
|---|---|---|---|
| libbirriasdr, birria_burst | v0.1.0-flipperone.3 | MIT, with parts under BSD-3-Clause-Clear | https://github.com/flipperdevices/BirriaSDR/tree/v0.1.0-flipperone.3 |

libbirriasdr loads libusb, which the bundle carries unmodified from Debian trixie
(`libusb-1.0-0` 2:1.0.28-1, LGPL-2.1-or-later, `apt-get source libusb-1.0`). Its
copyright file is under `usr/share/doc/libusb-1.0-0/`.

The radio and decoder in `src/` (`radio.rs`, `front.rs`, `channels.rs`, `post.rs`,
`pin.rs`, `weave.rs` and `dsp/`) come from FlipperOne-WiFi-SDR-FPV-receiver, MIT,
changed where those files say.
