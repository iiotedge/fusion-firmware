#!/usr/bin/env python3
"""Every virtual sensor of an endpoint set has a resting value from the moment the
firmware starts.

  initial-signals.py SET.toml                          # static: the set is complete
  initial-signals.py SET.toml --http URL --token T     # + live: the running firmware serves them

Static: every `push:<name>` an endpoint READS (`source`, or each of `sources`) has an entry in
the set's `[signals.initial]` table. The strict attribute sweep cannot prove this on its own:
a sensor without a reading answers Failure only on attributes that cannot be null (a leak
sensor's StateValue), while a measurement (pressure, light level, ...) reads as null and
counts as a clean read - a controller app still shows that tile as "No Response".

Live: with the firmware booted from the set and NOTHING pushed yet, `GET /signals` already
holds every `[signals.initial]` value, equal to the one configured. That is the wiring
from the config table to the signal bus, which a unit test of the bus cannot see.

Prints PASS/FAIL lines and an `N/M checks passed` summary like the other harness scripts;
the exit status is 0 only when every check passed.
"""
import argparse
import json
import sys
import tomllib
import urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("toml")
ap.add_argument("--http", help="base URL of the running firmware, e.g. http://127.0.0.1:9100")
ap.add_argument("--token", default="", help="its command_token")
args = ap.parse_args()

with open(args.toml, "rb") as f:
    cfg = tomllib.load(f)
initial = cfg.get("signals", {}).get("initial", {})
endpoints = cfg.get("matter", {}).get("endpoints", [])

results = []


def check(ok, text):
    results.append(ok)
    print(("PASS  " if ok else "FAIL  ") + text)


def reads(endpoint):
    """The `push:` names an endpoint reads, with the label to blame them on."""
    sources = [endpoint["source"]] if "source" in endpoint else []
    sources += list(endpoint.get("sources", {}).values())
    return [s.removeprefix("push:") for s in sources if isinstance(s, str) and s.startswith("push:")]


wanted = {}
for e in endpoints:
    for name in reads(e):
        wanted.setdefault(name, f"endpoint {e.get('endpoint', '?')} '{e.get('name', e.get('kind', '?'))}'")

missing = sorted(set(wanted) - set(initial))
for name in missing:
    check(False, f"push:{name} ({wanted[name]}) has no resting value in [signals.initial]")
if not missing:
    check(True, f"every push: source of the endpoint set has a resting value in [signals.initial] ({len(wanted)} sources)")

if args.http:
    req = urllib.request.Request(args.http.rstrip("/") + "/signals")
    if args.token:
        req.add_header("Authorization", f"Bearer {args.token}")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            live = json.load(r)
    except Exception as e:  # noqa: BLE001 - any failure to read is a failed check
        check(False, f"GET /signals failed: {e}")
        live = None
    if live is not None:
        wrong = []
        for name, want in initial.items():
            got = live.get(f"push:{name}")
            same = (
                got is not None
                and isinstance(got, bool) == isinstance(want, bool)
                and (got == want if isinstance(want, bool) else abs(float(got) - float(want)) < 1e-9)
            )
            if not same:
                wrong.append(f"push:{name} is {json.dumps(got)}, [signals.initial] says {json.dumps(want)}")
        for line in wrong:
            check(False, line)
        if not wrong:
            check(True, f"the running firmware reads every [signals.initial] value before anything is pushed ({len(initial)} signals)")

print(f"{sum(results)}/{len(results)} checks passed")
sys.exit(0 if all(results) else 1)
