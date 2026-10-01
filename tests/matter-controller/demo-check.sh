#!/usr/bin/env bash
# Validate the demo endpoint set (demo-endpoints.toml) against a REAL Matter controller:
# boot the firmware with it, give every virtual sensor a first reading, then have
# matter.js commission the node and read EVERY attribute each cluster advertises plus
# the five mandatory global ones, on every endpoint (device.mjs --sweep-only).
#
# The real sources (SoC temperature, camera flags, camera AI, CPU load ...) do not exist
# on a dev host, and the sweep checks structure and conformance, which depend on the
# endpoint's KIND and not on where its value comes from: so each is swapped for a push
# source of the same type. The set also pins its ids out of order on purpose - rs-matter
# panics at the first wildcard read unless the firmware sorts them.
#
#   tests/matter-controller/demo-check.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOCAL="$(mktemp "${TMPDIR:-/tmp}/fusion-demo-endpoints.XXXXXX")"
trap 'rm -f "$LOCAL"' EXIT
python3 - "$HERE/demo-endpoints.toml" "$LOCAL" <<'PY'
import re, sys
src, dst = sys.argv[1:3]
swap = {"temperature_sensor": "push:test_temp", "humidity_sensor": "push:test_humidity",
        "contact_sensor": "push:test_door", "occupancy_sensor": "push:test_pir"}
out = []
for block in re.split(r'(?m)^(?=\[\[matter\.endpoints\]\])', open(src).read()):
    kind = re.search(r'(?m)^kind\s*=\s*"(\w+)"', block)
    if kind and re.search(r'(?m)^source\s*=\s*"(builtin|sysfs|ai):', block):
        block = re.sub(r'(?m)^source\s*=\s*"[^"]*"', f'source = "{swap[kind.group(1)]}"', block)
        block = re.sub(r'(?m)^scale\s*=.*\n', '', block)
    out.append(block)
open(dst, "w").write("".join(out))
PY
echo "--- demo endpoint set, strict sweep"
MATTER_SWEEP_ONLY=1 MATTER_EXTRA_TOML="$LOCAL" "$HERE/run.sh"
