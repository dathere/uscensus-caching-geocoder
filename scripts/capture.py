#!/usr/bin/env python3
"""Throttled (2 req/s) capture of live Census Geocoder data.

  scripts/capture.py catalog   -> resources/{catalog_snapshot,default_layers,tigerweb_layers}.json
  scripts/capture.py samples   -> resources/samples/*.json  (~500 raw responses for train-dict)

Read-only GETs only. Re-run when Census rotates benchmarks/vintages (twice a year).
"""

import json
import random
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

BASE = "https://geocoding.geo.census.gov/geocoder"
TIGERWEB = "https://tigerweb.geo.census.gov/arcgis/rest/services/TIGERweb"
UA = "ucgeocoder-dev-capture/0.1 (+https://github.com/dathere/uscensus-caching-geocoder)"
ROOT = Path(__file__).resolve().parent.parent
RES = ROOT / "resources"
MIN_GAP = 0.5  # seconds between request starts => 2 req/s

_last = 0.0


def get(url, params=None):
    global _last
    if params:
        url = url + "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    for attempt in range(4):
        # Throttle every attempt, retries included, so starts stay >= MIN_GAP apart.
        wait = _last + MIN_GAP - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        _last = time.monotonic()
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < 3:
                time.sleep(5 * 2**attempt)
                continue
            return e.code, e.read()
        except (urllib.error.URLError, TimeoutError) as e:
            if attempt < 3:
                time.sleep(5 * 2**attempt)
                continue
            print(f"  giving up on {url}: {e}", file=sys.stderr)
            return None, b""
    return None, b""


def get_json(url, params=None):
    status, body = get(url, params)
    if status != 200:
        raise SystemExit(f"{url} {params} -> HTTP {status}: {body[:200]!r}")
    return json.loads(body)


# ---------------------------------------------------------------- catalog

PROBE_POINTS = [(-77.0365, 38.8977), (-76.92744, 38.84601)]  # DC (incorporated), Suitland CDP


def normalize_default_layers(default_layers):
    """Probe points only reveal layers that cover them. Fill in the mutually exclusive
    partners named by the Census API docs (PLAN.md §11.1) and drop vintages that
    returned nothing (an empty list would filter out every layer)."""
    pairs = [
        ("Incorporated Places", "Census Designated Places"),
        ("Urbanized Areas", "Urban Clusters"),
        ("2010 Census Urbanized Areas", "2010 Census Urban Clusters"),  # ACS2017-ACS2019 names
    ]
    out = {}
    for name, layers in default_layers.items():
        if not layers:
            continue
        layers = list(layers)
        for a, b in pairs:
            if a in layers and b not in layers:
                layers.append(b)
            elif b in layers and a not in layers:
                layers.append(a)
        out[name] = layers
    return out


def capture_catalog():
    benchmarks = get_json(f"{BASE}/benchmarks")
    vintages = {}
    for b in benchmarks["benchmarks"]:
        vintages[b["id"]] = get_json(f"{BASE}/vintages", {"benchmark": b["id"]})
        print(f"benchmark {b['id']} {b['benchmarkName']}: {len(vintages[b['id']]['vintages'])} vintages")
    snapshot = {"captured": time.strftime("%Y-%m-%d"), "benchmarks": benchmarks, "vintages": vintages}
    (RES / "catalog_snapshot.json").write_text(json.dumps(snapshot, indent=1) + "\n")

    default_layers = {}
    suffixes = set()
    for bid, vs in vintages.items():
        for v in vs["vintages"]:
            name = v["vintageName"]
            suffixes.add(name.split("_", 1)[0])
            if name in default_layers:
                continue
            names = []
            for x, y in PROBE_POINTS:
                status, body = get(f"{BASE}/geographies/coordinates",
                                   {"x": x, "y": y, "benchmark": bid, "vintage": v["id"], "format": "json"})
                if status != 200:
                    print(f"  {name}: HTTP {status}", file=sys.stderr)
                    continue
                for layer in json.loads(body)["result"]["geographies"]:
                    if layer not in names:
                        names.append(layer)
            default_layers[name] = names
            print(f"vintage {name}: {len(names)} default layers")
    (RES / "default_layers.json").write_text(json.dumps(normalize_default_layers(default_layers), indent=1) + "\n")

    tiger = {}
    for suffix in sorted(suffixes):
        status, body = get(f"{TIGERWEB}/tigerWMS_{suffix}/MapServer", {"f": "json"})
        if status != 200:
            print(f"tigerWMS_{suffix}: HTTP {status}", file=sys.stderr)
            continue
        doc = json.loads(body)
        if "layers" not in doc:
            print(f"tigerWMS_{suffix}: no layers ({str(doc)[:120]})", file=sys.stderr)
            continue
        tiger[suffix] = {str(l["id"]): l["name"] for l in doc["layers"] if "Labels" not in l["name"]}
        print(f"tigerWMS_{suffix}: {len(tiger[suffix])} layers")
    (RES / "tigerweb_layers.json").write_text(json.dumps(tiger, indent=1, sort_keys=False) + "\n")


# ---------------------------------------------------------------- samples

ADDRESSES = [
    ("600 Dexter Ave", "Montgomery", "AL", "36104"), ("120 4th St", "Juneau", "AK", "99801"),
    ("1700 W Washington St", "Phoenix", "AZ", "85007"), ("500 Woodlane St", "Little Rock", "AR", "72201"),
    ("1315 10th St", "Sacramento", "CA", "95814"), ("200 E Colfax Ave", "Denver", "CO", "80203"),
    ("210 Capitol Ave", "Hartford", "CT", "06106"), ("411 Legislative Ave", "Dover", "DE", "19901"),
    ("400 S Monroe St", "Tallahassee", "FL", "32399"), ("206 Washington St SW", "Atlanta", "GA", "30334"),
    ("415 S Beretania St", "Honolulu", "HI", "96813"), ("700 W Jefferson St", "Boise", "ID", "83702"),
    ("401 S 2nd St", "Springfield", "IL", "62701"), ("200 W Washington St", "Indianapolis", "IN", "46204"),
    ("1007 E Grand Ave", "Des Moines", "IA", "50319"), ("300 SW 10th Ave", "Topeka", "KS", "66612"),
    ("700 Capital Ave", "Frankfort", "KY", "40601"), ("900 N 3rd St", "Baton Rouge", "LA", "70802"),
    ("210 State St", "Augusta", "ME", "04330"), ("100 State Cir", "Annapolis", "MD", "21401"),
    ("24 Beacon St", "Boston", "MA", "02133"), ("100 N Capitol Ave", "Lansing", "MI", "48933"),
    ("75 Rev Dr Martin Luther King Jr Blvd", "St Paul", "MN", "55155"), ("400 High St", "Jackson", "MS", "39201"),
    ("201 W Capitol Ave", "Jefferson City", "MO", "65101"), ("1301 E 6th Ave", "Helena", "MT", "59601"),
    ("1445 K St", "Lincoln", "NE", "68508"), ("101 N Carson St", "Carson City", "NV", "89701"),
    ("107 N Main St", "Concord", "NH", "03301"), ("125 W State St", "Trenton", "NJ", "08608"),
    ("490 Old Santa Fe Trl", "Santa Fe", "NM", "87501"), ("1 Centre St", "New York", "NY", "10007"),
    ("1 E Edenton St", "Raleigh", "NC", "27601"), ("600 E Boulevard Ave", "Bismarck", "ND", "58505"),
    ("1 Capitol Sq", "Columbus", "OH", "43215"), ("2300 N Lincoln Blvd", "Oklahoma City", "OK", "73105"),
    ("900 Court St NE", "Salem", "OR", "97301"), ("501 N 3rd St", "Harrisburg", "PA", "17120"),
    ("82 Smith St", "Providence", "RI", "02903"), ("1100 Gervais St", "Columbia", "SC", "29201"),
    ("500 E Capitol Ave", "Pierre", "SD", "57501"), ("600 Dr Martin L King Jr Blvd", "Nashville", "TN", "37243"),
    ("1100 Congress Ave", "Austin", "TX", "78701"), ("350 N State St", "Salt Lake City", "UT", "84103"),
    ("115 State St", "Montpelier", "VT", "05633"), ("1000 Bank St", "Richmond", "VA", "23219"),
    ("416 Sid Snyder Ave SW", "Olympia", "WA", "98504"), ("1900 Kanawha Blvd E", "Charleston", "WV", "25305"),
    ("2 E Main St", "Madison", "WI", "53703"), ("200 W 24th St", "Cheyenne", "WY", "82001"),
    ("1600 Pennsylvania Ave NW", "Washington", "DC", "20500"), ("4600 Silver Hill Rd", "Washington", "DC", "20233"),
    ("350 5th Ave", "New York", "NY", "10118"), ("233 S Wacker Dr", "Chicago", "IL", "60606"),
    ("600 Montgomery St", "San Francisco", "CA", "94111"), ("400 Broad St", "Seattle", "WA", "98109"),
    ("1000 5th Ave", "New York", "NY", "10028"), ("1 Microsoft Way", "Redmond", "WA", "98052"),
    ("2800 E Observatory Rd", "Los Angeles", "CA", "90027"), ("1 Lincoln Memorial Cir NW", "Washington", "DC", "20002"),
]

CITIES = [
    (40.71, -74.00), (34.05, -118.24), (41.88, -87.63), (29.76, -95.37), (33.45, -112.07), (39.95, -75.17),
    (29.42, -98.49), (32.72, -117.16), (32.78, -96.80), (37.34, -121.89), (30.27, -97.74), (30.33, -81.66),
    (32.76, -97.33), (39.96, -83.00), (35.23, -80.84), (37.77, -122.42), (39.77, -86.16), (47.61, -122.33),
    (39.74, -104.99), (38.91, -77.04), (42.36, -71.06), (31.76, -106.49), (36.16, -86.78), (42.33, -83.05),
    (35.47, -97.52), (45.52, -122.68), (36.17, -115.14), (35.15, -90.05), (38.25, -85.76), (39.29, -76.61),
    (43.04, -87.91), (35.08, -106.65), (32.22, -110.97), (36.74, -119.79), (38.58, -121.49), (33.75, -84.39),
    (39.10, -94.58), (41.26, -95.93), (25.76, -80.19), (44.98, -93.27), (36.15, -95.99), (41.50, -81.69),
    (27.95, -82.46), (29.95, -90.07), (37.69, -97.34), (40.76, -111.89), (43.62, -116.20), (46.87, -96.79),
    (44.08, -103.23), (46.59, -112.04), (41.14, -104.82), (61.22, -149.90), (21.31, -157.86), (18.47, -66.11),
    (44.48, -73.21), (43.66, -70.26), (43.21, -71.54), (41.82, -71.41), (41.76, -72.67), (39.74, -75.55),
    (38.35, -81.63), (37.54, -77.44), (35.78, -78.64), (34.00, -81.03), (32.08, -81.09), (30.44, -84.28),
    (32.37, -86.30), (32.30, -90.18), (34.75, -92.29), (30.45, -91.19), (38.63, -90.20), (41.59, -93.62),
    (39.05, -95.68), (40.81, -96.70), (46.81, -100.78), (44.37, -100.35), (47.66, -117.43), (44.05, -123.09),
    (39.53, -119.81), (35.20, -101.83), (31.55, -97.15), (40.44, -79.99), (42.89, -78.88), (43.16, -77.61),
    (42.65, -73.75), (40.22, -74.76), (39.16, -84.51), (41.08, -85.14), (42.73, -84.56), (43.07, -89.40),
    (41.66, -91.53), (40.11, -88.24), (37.21, -93.29), (36.06, -94.16), (35.96, -83.92), (36.85, -75.98),
    (64.84, -147.72), (19.71, -155.08), (48.76, -122.48), (27.80, -97.40),
]

NOMATCH = [
    "zzzz nowhere", "123 Fake Street, Springfield", "qwerty asdf", "1 Nonexistent Blvd, Nowhere, ZZ 00000",
    "99999 Imaginary Rd, Atlantis", "the moon", "PO Box 1, Nowhere", "0 Null Ave", "xyz 123", "lorem ipsum dolor",
    "999999 Main St, Anytown, XX", "1 Infinite Loop Mars", "abc def ghi", "77 Sunset Strip, Gotham",
    "42 Wallaby Way, Sydney", "221B Baker Street, London", "4 Privet Drive, Little Whinging", "12 Grimmauld Place",
    "742 Evergreen Terrace, Springfield, ZZ", "1313 Mockingbird Ln, Mockingbird Heights", "unknown", "N/A",
    "test test test", "000 000 000", "Apt 4B", "Suite 100", "corner of nowhere and somewhere",
    "1 Rue de Rivoli, Paris", "10 Downing Street, London", "Platform 9 3/4",
]

LOCATIONS_ONELINE = [f"{s}, {c}, {st} {z}" for s, c, st, z in ADDRESSES[::3]]


def save(n, kind, status, body):
    if status != 200 or not body:
        print(f"  {n:04d} {kind}: HTTP {status}", file=sys.stderr)
        return False
    (RES / "samples" / f"{n:04d}_{kind}.json").write_bytes(body)
    return True


def capture_samples():
    rng = random.Random(20261012)
    out = RES / "samples"
    out.mkdir(parents=True, exist_ok=True)
    vintages = json.loads((RES / "catalog_snapshot.json").read_text())["vintages"]

    def default_vintage(bid):
        return next(v["id"] for v in vintages[bid]["vintages"] if v["isDefault"])

    combos = [("4", "4")] * 6 + [("4", "426"), ("4", "420"), ("4", "410")]
    combos += [(b, default_vintage(b)) for b in ("8", "2020", "11") if b in vintages]

    jobs = []
    for s, c, st, z in ADDRESSES:
        jobs.append(("addr", {"street": s, "city": c, "state": st, "zip": z, "benchmark": "4", "vintage": "4"}))
    for s, c, st, z in ADDRESSES[::2]:
        b, v = rng.choice(combos[6:])
        jobs.append(("addr", {"street": s, "city": c, "state": st, "benchmark": b, "vintage": v}))
    for s, c, st, z in ADDRESSES[1::2]:
        jobs.append(("oneline", {"address": f"{s}, {c}, {st} {z}", "benchmark": "4", "vintage": "4"}))
    for lat, lon in CITIES * 3:
        b, v = rng.choice(combos)
        x = round(lon + rng.uniform(-0.15, 0.15), 5)
        y = round(lat + rng.uniform(-0.15, 0.15), 5)
        jobs.append(("coords", {"x": x, "y": y, "benchmark": b, "vintage": v}))
    for a in NOMATCH:
        jobs.append(("nomatch", {"address": a, "benchmark": "4", "vintage": "4"}))
    for a in LOCATIONS_ONELINE:
        jobs.append(("locations", {"address": a, "benchmark": "4"}))

    ok = 0
    for n, (kind, p) in enumerate(jobs):
        if kind == "locations":
            url = f"{BASE}/locations/onelineaddress"
        elif kind == "coords":
            url = f"{BASE}/geographies/coordinates"
        elif kind == "addr":
            url = f"{BASE}/geographies/address"
        else:
            url = f"{BASE}/geographies/onelineaddress"
        params = dict(p, format="json")
        if kind != "locations":
            params["layers"] = "all"
        status, body = get(url, params)
        ok += save(n, kind, status, body)
        if n % 25 == 0:
            print(f"{n}/{len(jobs)} ({ok} ok)", flush=True)
    print(f"done: {ok}/{len(jobs)} samples in {out}")


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if mode == "catalog":
        capture_catalog()
    elif mode == "samples":
        capture_samples()
    else:
        raise SystemExit(__doc__)
