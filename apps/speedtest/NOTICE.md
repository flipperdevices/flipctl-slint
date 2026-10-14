# Programs this bundle carries

The measuring is done by these, unmodified. Each one's license text travels beside
it under `usr/share/doc/<program>/`.

| Program | Version | License | Source |
|---|---|---|---|
| speedtest-go | 1.8.3 | MIT | https://github.com/showwin/speedtest-go/tree/v1.8.3 |
| librespeed-cli | 1.0.14 | LGPL-3.0 | https://github.com/librespeed/speedtest-cli/tree/v1.0.14 |
| cfspeedtest | 2.2.2 | MIT | https://github.com/code-inflation/cfspeedtest/tree/v2.2.2 |
| iperf3, libiperf0 | 3.18-2+deb13u2 | BSD-3-Clause | Debian trixie, `apt-get source iperf3` |
| libsctp1 | 1.0.21+dfsg-1 | LGPL-2.1 | Debian trixie, `apt-get source lksctp-tools` |
| libssl3t64 | 3.5.7-1~deb13u3 | Apache-2.0 | Debian trixie, `apt-get source openssl` |

The city table Ookla is asked about, `cities.tsv`, is compiled into the program. Its
names and coordinates are from GeoNames (https://www.geonames.org), used under
CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/), picked and cut down by
`tools/tables.py`; the license text is in `LICENSES/CC-BY-4.0.txt`.

The public iperf3 servers, `iperf3-servers.tsv`, are compiled in too. They are the
export of https://github.com/R0GGER/public-iperf3-servers (iperf3serverlist.net),
under the MIT license, cut down by `tools/tables.py`.

The first three are the projects' own release builds, fetched and checked by
checksum from their release pages (`release.lock`); the rest are Debian's packages
(`deb.lock`), whose copyright files travel under `usr/share/doc/<package>/`.
