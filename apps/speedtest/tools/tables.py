#!/usr/bin/env python3
"""Write the speed test's two tables of places, from their sources' own exports.

    python3 -I apps/speedtest/tools/tables.py cities cities15000.txt countryInfo.txt \
        > apps/speedtest/cities.tsv
    python3 -I apps/speedtest/tools/tables.py iperf3 listed_iperf3_servers.json countryInfo.txt \
        > apps/speedtest/iperf3-servers.tsv

Both name countries the same way, GeoNames' own, so a country reads the same
whichever service is chosen; and both spell everything in ASCII, since the panel's
fonts are ASCII.

cities: the places Ookla can be asked about. The GeoNames inputs are from
https://download.geonames.org/export/dump/, under CC BY 4.0. speedtest-go knows 26
cities of its own and takes any other place as coordinates, so the app lists the
places it can name and passes the one chosen with --location.

Per country: its capital, then the largest city of each of its first-level regions
(a state, a nation of the UK), then its other largest cities, all of at least
MIN_POPULATION and PER_COUNTRY in all. By region first because a big country's
largest cities bunch together, and a list of New York's boroughs would leave the
rest of the country out. A country with none of these has its largest city, so
every country in the dump can be chosen. A place within NEAR_KM of a larger one
already chosen is a part of it, Belgrade's Zemun or New York's Brooklyn, and is
left out: GeoNames tags those the way it tags a town of their size. A record known
to be wrong is in WRONG and left out.

Columns: country code, country, city, latitude, longitude, 1 for the capital.
Countries by name, each country's cities by name.

iperf3: public iperf3 servers, from https://export.iperf3serverlist.net, the export
of https://github.com/R0GGER/public-iperf3-servers (MIT). It drops a server whose
uptime falls under 90% over a month, so a snapshot ages well enough between builds.

Columns: country, city, host, port or first-last range, provider, numbered where a
provider has more than one server in a city. By country, city and provider.
"""

import json

import math
import sys
import unicodedata

PER_COUNTRY = 12
MIN_POPULATION = 150_000
NEAR_KM = 15

# A district of a city, and places that are not lived in any more.
SKIPPED = {"PPLX", "PPLH", "PPLQ", "PPLW"}

# Records whose numbers are wrong in a way no rule here can see. Rasapudipalem is a
# suburb of Visakhapatnam counted at more than the city itself, so it would take
# the city's place and the distance rule would then drop the city.
WRONG = {"Rasapudipalem"}

# GeoNames' column numbers, from its readme.
NAME_ASCII, LAT, LON, FEATURE, COUNTRY, REGION, POPULATION = 2, 4, 5, 7, 8, 10, 14


def ascii_only(text: str) -> str:
    """Letters with marks as the letter underneath, and anything else dropped."""
    plain = unicodedata.normalize("NFKD", text).encode("ascii", "ignore").decode()
    return " ".join(plain.split())


def km(a: tuple[float, float], b: tuple[float, float]) -> float:
    """Great-circle distance, which at 15km is all the precision wanted."""
    lat1, lon1, lat2, lon2 = map(math.radians, (*a, *b))
    h = math.sin((lat2 - lat1) / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin((lon2 - lon1) / 2) ** 2
    return 6371 * 2 * math.asin(math.sqrt(h))


def countries(path: str) -> dict[str, str]:
    names = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.startswith("#"):
                continue
            fields = line.rstrip("\n").split("\t")
            if len(fields) > 4:
                names[fields[0]] = ascii_only(fields[4])
    return names


def cities(cities_path: str, countries_path: str) -> None:
    names = countries(countries_path)
    by_country: dict[str, list[tuple[int, bool, str, str, str, str]]] = {}
    with open(cities_path, encoding="utf-8") as f:
        for line in f:
            fields = line.rstrip("\n").split("\t")
            city = ascii_only(fields[NAME_ASCII])
            if not city or city in WRONG or fields[COUNTRY] not in names or fields[FEATURE] in SKIPPED:
                continue
            capital = fields[FEATURE] == "PPLC"
            by_country.setdefault(fields[COUNTRY], []).append(
                (int(fields[POPULATION] or 0), capital, city, fields[LAT], fields[LON], fields[REGION])
            )

    rows = []
    for code, cities in by_country.items():
        cities.sort(key=lambda c: -c[0])
        big = [c for c in cities if c[0] >= MIN_POPULATION]
        regions_first, regions = [], set()
        for c in big:
            if c[5] not in regions:
                regions.add(c[5])
                regions_first.append(c)
        order = [c for c in cities if c[1]] + regions_first + big + cities[:1]
        chosen, seen = [], set()
        for _, capital, city, lat, lon, _ in order:
            here = (float(lat), float(lon))
            near = any(km(here, (float(c[1]), float(c[2]))) < NEAR_KM for c in chosen)
            if len(chosen) < PER_COUNTRY and city not in seen and not near:
                chosen.append((city, lat, lon, capital))
                seen.add(city)
        for city, lat, lon, capital in chosen:
            rows.append((names[code], city, code, lat, lon, capital))

    rows.sort(key=lambda r: (r[0], r[1]))
    for country, city, code, lat, lon, capital in rows:
        print(f"{code}\t{country}\t{city}\t{float(lat):.4f}\t{float(lon):.4f}\t{int(capital)}")


def iperf3(servers_path: str, countries_path: str) -> None:
    names = countries(countries_path)
    with open(servers_path, encoding="utf-8") as f:
        servers = json.load(f)
    rows = set()
    for server in servers:
        country = names.get(server["COUNTRY"], "")
        host = server["IP/HOST"].strip()
        # Every server listed takes -R today, and one that did not would measure
        # only half of what the app asks of it.
        if not country or not host or "-R" not in server["OPTIONS"]:
            continue
        rows.add(
            (
                country,
                ascii_only(server["SITE"]),
                host,
                server["PORT"].strip() or "5201",
                ascii_only(server["PROVIDER"]),
            )
        )
    # One provider with two servers in a city would read as one row twice, so each
    # is numbered, the way LibreSpeed's own list numbers Clouvider's in Los Angeles.
    rows = sorted(rows, key=lambda r: (r[0], r[1], r[4], r[2]))
    seen: dict[tuple[str, str, str], int] = {}
    for country, city, _, _, provider in rows:
        seen[(country, city, provider)] = seen.get((country, city, provider), 0) + 1
    count: dict[tuple[str, str, str], int] = {}
    for country, city, host, ports, provider in rows:
        key = (country, city, provider)
        if seen[key] > 1:
            count[key] = count.get(key, 0) + 1
            provider = f"{provider} {count[key]}"
        print("\t".join((country, city, host, ports, provider)))


if __name__ == "__main__":
    commands = {"cities": cities, "iperf3": iperf3}
    if len(sys.argv) != 4 or sys.argv[1] not in commands:
        sys.exit(__doc__)
    commands[sys.argv[1]](sys.argv[2], sys.argv[3])
