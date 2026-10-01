#!/usr/bin/env bash
# Commission a freshly-booted fusion-firmware with a REAL, independent Matter
# controller (matter.js) and exercise every endpoint through the Matter
# Interaction Model. Exit status is the harness's (0 = every check passed).
#
#   tests/matter-controller/run.sh                 # camera+onoff+light+thermostat
#   MATTER_CAMERA=false tests/matter-controller/run.sh
#   MATTER_EXTRA_TOML=extra.toml tests/matter-controller/run.sh   # replaces endpoints.toml (empty = none)
#   FUSION_BIN=path/to/fusion-firmware tests/matter-controller/run.sh
#
# Why this exists: cargo check/clippy/test and a clean boot prove the code
# compiles and constructs; they say nothing about whether real Matter reads,
# writes, commands and change reports are routed correctly. Hand-written
# clusters in particular have no crate-provided conformance check.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BIN="${FUSION_BIN:-$REPO/target/debug/fusion-firmware}"
[ -x "$BIN" ] || { echo "firmware binary not found: $BIN (run 'cargo build' first)" >&2; exit 2; }

command -v node >/dev/null || { echo "node is required" >&2; exit 2; }
if [ ! -d "$HERE/node_modules" ]; then
  echo "installing matter.js (first run)..."
  (cd "$HERE" && npm install --no-audit --no-fund >/dev/null)
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/fusion-matter-verify.XXXXXX")"
mkdir -p "$WORK/config"
cp "$REPO/config/iiotedge_default.toml" "$WORK/config/"
python3 - "$WORK/config/iiotedge_default.toml" \
  "${MATTER_CAMERA:-true}" "${MATTER_ONOFF:-true}" "${MATTER_LIGHT:-true}" "${MATTER_THERMOSTAT:-true}" \
  "${MATTER_EXTRA_TOML-$HERE/endpoints.toml}" <<'PY'
import re, sys
p, cam, onoff, light, thermo, extra = sys.argv[1:7]
s = open(p).read()
# A known bearer token so the harness can POST /signals/<name>.
s = re.sub(r'(?m)^command_token = ".*?"', 'command_token = "fusion-verify-token"', s)
def sec(name, val):
    global s
    for cur in ("false", "true"):
        s = s.replace(f"[{name}]\nenabled = {cur}", f"[{name}]\nenabled = {val}")
# A configured vendor name (product name is left to derive from what is enabled).
s = s.replace("[matter]\nenabled = false", '[matter]\nenabled = true\nvendor_name = "Acme Controls"')
sec("matter.camera", cam); sec("matter.onoff", onoff)
sec("matter.light", light); sec("matter.thermostat", thermo)
if extra:
    s += "\n" + open(extra).read()
open(p, "w").write(s)
PY

# A Modbus TCP simulator, and the SDK engine config pointing the firmware's REAL
# Modbus driver at it: `[[tags]]` turns the registers into signals and Matter
# endpoints read those, so southbound machine data -> Matter is exercised end to end.
node "$HERE/modbus-sim.mjs" --port 5020 --control 5021 > "$WORK/modbus-sim.log" 2>&1 &
SIM_PID=$!
cp "$REPO/config/edge.toml" "$WORK/config/edge.toml"
cat >> "$WORK/config/edge.toml" <<'EDGE'

[[southbound.modbus]]
name = "sim"
host = "127.0.0.1"
port = 5020
unit_id = 1
poll_interval_ms = 200
  [[southbound.modbus.reads]]
  name = "block"
  function = "holding"
  address = 0
  count = 8
  [[southbound.modbus.reads]]
  name = "relays"
  function = "coil"
  address = 0
  count = 4
EDGE

cp "$BIN" "$WORK/fusion-firmware"
cd "$WORK"
RUST_LOG=info ./fusion-firmware > firmware.log 2>&1 &
FW_PID=$!
cleanup() {
  kill "$FW_PID" "$SIM_PID" 2>/dev/null || true
  wait "$FW_PID" "$SIM_PID" 2>/dev/null || true
}
trap cleanup EXIT

for _ in $(seq 1 30); do
  grep -q "Running Matter transport" firmware.log 2>/dev/null && break
  kill -0 "$FW_PID" 2>/dev/null || { echo "firmware exited early:" >&2; tail -20 firmware.log >&2; exit 2; }
  sleep 1
done

set +e
SW_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
(cd "$HERE" && node controller.mjs --ip 127.0.0.1 --port 5540 --sw-version "$SW_VERSION" 2>&1 \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E '^(PASS|FAIL|commissioning|endpoints:|  endpoint|[0-9]+/[0-9]+ checks)')
CODE=${PIPESTATUS[0]}
set -e

echo "--- firmware ERROR/panic lines (excluding expected noise) ---"
sed 's/\x1b\[[0-9;]*m//g' firmware.log \
  | grep -E 'ERROR|panick' \
  | grep -vE 'Telemetry engine|AI engine|UnsupportedAttribute|AttributeNotFound|ConstraintError|is synthetic \(mock' || echo "(none)"
echo "firmware log kept at $WORK/firmware.log"
exit "$CODE"
