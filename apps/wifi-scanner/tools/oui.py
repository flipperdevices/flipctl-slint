#!/usr/bin/env python3
"""Turn the IEEE OUI registry into the Wi-Fi Scanner's vendor table.

    apps/wifi-scanner/tools/oui.py oui.txt > apps/wifi-scanner/oui.tsv

oui.txt is the MA-L listing, from https://standards-oui.ieee.org/oui/oui.txt or
Debian's ieee-data (/usr/share/ieee-data/oui.txt). Each line of the table is the
prefix in hex and the registered name, sorted by prefix, which the app embeds and so
needs nothing installed. IEEE asserts no copyright in the listing.
"""

import re
import sys

LINE = re.compile(r"^([0-9A-F]{2})-([0-9A-F]{2})-([0-9A-F]{2})\s+\(hex\)\s+(.*?)\s*$")

names = {}
with open(sys.argv[1], encoding="utf-8", errors="replace") as f:
    for line in f:
        m = LINE.match(line)
        if m and m.group(4):
            names.setdefault("".join(m.group(1, 2, 3)), m.group(4))

out = sys.stdout
for prefix in sorted(names):
    out.write(f"{prefix}\t{names[prefix]}\n")
