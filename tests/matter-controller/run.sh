#!/usr/bin/env bash
# Commission a freshly-booted fusion-firmware with a REAL, independent Matter
# controller (matter.js) and exercise every endpoint through the Matter
# Interaction Model. Exit status is the harness's (0 = every check passed).
#
#   tests/matter-controller/run.sh                 # camera+onoff+light+thermostat
#   MATTER_CAMERA=false tests/matter-controller/run.sh
#   MATTER_EXTRA_TOML=extra.toml tests/matter-controller/run.sh   # replaces endpoints.toml (empty = none)
#   FUSION_BIN=path/to/fusion-firmware tests/matter-controller/run.sh
#   FW_LOG=debug tests/matter-controller/run.sh      # a verbose firmware log, for diagnosing
#   MATTER_SCRIPT=multiadmin.mjs tests/matter-controller/run.sh   # boot, then run just that one script
#                                                     # (given --ip --port --qr --manual and $MATTER_SCRIPT_ARGS)
#   MATTER_KEYS=$'setup_passcode = 31415926'  ...      # extra keys for the [matter] section
#   MATTER_SKIP_DEMO=1 tests/matter-controller/run.sh # skip the final demo-endpoint-set sweep (demo-check.sh)
#   MATTER_SWEEP_ONLY=1 MATTER_EXTRA_TOML=demo.toml tests/matter-controller/run.sh
#                                                     # boot with ANY endpoint set, seed its virtual sensors
#                                                     # (demo-device.mjs --seed) and run only the strict
#                                                     # attribute sweep (device.mjs --sweep-only)
#   MATTER_NO_SEED=1 ...                              # with MATTER_SWEEP_ONLY: do not seed; the endpoint set
#                                                     # must carry its own [signals.initial] resting values,
#                                                     # which the sweep then proves (a sensor without one
#                                                     # answers Failure and is reported unreadable)
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
  "${MATTER_EXTRA_TOML-$HERE/endpoints.toml}" "${MATTER_KEYS:-}" <<'PY'
import re, sys
p, cam, onoff, light, thermo, extra, keys = sys.argv[1:8]
s = open(p).read()
# A known bearer token so the harness can POST /signals/<name>.
s = re.sub(r'(?m)^command_token = ".*?"', 'command_token = "fusion-verify-token"', s)
def sec(name, val):
    global s
    for cur in ("false", "true"):
        s = s.replace(f"[{name}]\nenabled = {cur}", f"[{name}]\nenabled = {val}")
# A configured vendor name (product name is left to derive from what is enabled).
if keys and "attestation" in keys:
    # the template already sets attestation = "test": the extra keys replace it
    s = re.sub(r'(?m)^attestation = "test".*\n', "", s, count=1)
s = s.replace("[matter]\nenabled = false", '[matter]\nenabled = true\nvendor_name = "Acme Controls"' + ("\n" + keys if keys else ""))
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
RUST_LOG="${FW_LOG:-info}" ./fusion-firmware > firmware.log 2>&1 &
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

if [ -n "${MATTER_SCRIPT:-}" ]; then
  # Run ONE harness script (e.g. multiadmin.mjs) against the freshly booted, addable node.
  SETUP="$(./fusion-firmware --matter-qr config/iiotedge_default.toml | sed 's/\x1b\[[0-9;]*m//g')"
  QR="$(printf '%s\n' "$SETUP" | sed -n 's/^QR payload: *//p')"
  MANUAL="$(printf '%s\n' "$SETUP" | sed -n 's/^Manual pairing code: *//p')"
  # shellcheck disable=SC2086
  (cd "$HERE" && node "$MATTER_SCRIPT" --ip 127.0.0.1 --port 5540 --qr "$QR" --manual "$MANUAL" ${MATTER_SCRIPT_ARGS:-} 2>&1 \
    | sed 's/\x1b\[[0-9;]*m//g' | grep -E '^(PASS|FAIL|[0-9]+/[0-9]+ checks)')
  RC=${PIPESTATUS[0]}
  echo "--- firmware ERROR/panic lines (excluding expected noise) ---"
  # (A script may deliberately try a wrong passcode: that PASE failure logs three ERROR lines.)
  sed 's/\x1b\[[0-9;]*m//g' firmware.log | grep -E 'ERROR|panick' | grep -vE 'Telemetry engine|AI engine|UnsupportedAttribute|AttributeNotFound|ConstraintError|is synthetic \(mock|Error (reading|writing) attribute: Failure|Invalid opcode: StatusReport|Status Report: StatusReport|exchange 0::0: Abandoned because of error Error::Invalid' | head -12 || true
  echo "firmware log kept at $WORK/firmware.log"
  exit "$RC"
fi

if [ -n "${MATTER_SWEEP_ONLY:-}" ]; then
  # An endpoint set the full checks know nothing about (a demo config): commission it
  # with matter.js, give every virtual sensor a first reading, and sweep EVERY attribute
  # each cluster advertises plus the five mandatory globals on every endpoint.
  SWEEP_RC=0
  if [ -z "${MATTER_NO_SEED:-}" ]; then
    (cd "$HERE" && FUSION_TOKEN=fusion-verify-token node demo-device.mjs --http http://127.0.0.1:9100 --seed)
  else
    # Nothing is pushed: the set's own [signals.initial] must already be complete and served
    # (the sweep alone passes a measurement with no reading - it reads as null).
    python3 "$HERE/initial-signals.py" "${MATTER_EXTRA_TOML:?MATTER_NO_SEED needs the endpoint set in MATTER_EXTRA_TOML}" \
      --http http://127.0.0.1:9100 --token fusion-verify-token || SWEEP_RC=1
  fi
  (cd "$HERE" && FUSION_TOKEN=fusion-verify-token node device.mjs --ip 127.0.0.1 --http http://127.0.0.1:9100 --sweep-only 2>&1 \
    | sed 's/\x1b\[[0-9;]*m//g' | grep -E '^(PASS|FAIL|sweep:|  unreadable|endpoints:|[0-9]+/[0-9]+ checks)') || SWEEP_RC=1
  exit "$SWEEP_RC"
fi

set +e
# The pairing lifecycle first: the setup code the operator is shown works, and the
# node can be removed from its last controller and added again with no restart. It
# leaves the node addable, which is what controller.mjs below starts from.
SETUP="$(./fusion-firmware --matter-qr config/iiotedge_default.toml | sed 's/\x1b\[[0-9;]*m//g')"
QR="$(printf '%s\n' "$SETUP" | sed -n 's/^QR payload: *//p')"
MANUAL="$(printf '%s\n' "$SETUP" | sed -n 's/^Manual pairing code: *//p')"
(cd "$HERE" && node lifecycle.mjs --ip 127.0.0.1 --port 5540 --qr "$QR" --manual "$MANUAL" --log "$WORK/firmware.log" 2>&1 \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E '^(PASS|FAIL|[0-9]+/[0-9]+ checks)')
LIFECYCLE_CODE=${PIPESTATUS[0]}

# Multi-admin: a node already in one ecosystem is added to a second one through an
# enhanced commissioning window the first opened (how Apple Home -> Google Home /
# Alexa / Home Assistant works). Two independent controllers, two fabrics; it leaves
# the node with no controller, which is what controller.mjs below starts from.
(cd "$HERE" && node multiadmin.mjs --ip 127.0.0.1 --port 5540 2>&1 \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E '^(PASS|FAIL|[0-9]+/[0-9]+ checks)')
MULTIADMIN_CODE=${PIPESTATUS[0]}

SW_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
(cd "$HERE" && node controller.mjs --ip 127.0.0.1 --port 5540 --sw-version "$SW_VERSION" --storage "$WORK/ctl" --keep 2>&1 \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -E '^(PASS|FAIL|commissioning|endpoints:|  endpoint|[0-9]+/[0-9]+ checks)')
CODE=${PIPESTATUS[0]}
[ "$LIFECYCLE_CODE" -eq 0 ] || CODE=$LIFECYCLE_CODE
[ "$MULTIADMIN_CODE" -eq 0 ] || CODE=$MULTIADMIN_CODE

# ConfigurationVersion follows the node's surface across restarts: the node
# controller.mjs left commissioned is restarted unchanged (version stays), then with
# one more endpoint in its config (version bumped by exactly one, the endpoint
# listed), then unchanged again (stays). The same state dir throughout, and the same
# controller storage, so this is what a paired controller sees after a config edit.
restart_firmware() {
  # Poll until the old process is really gone (its ports free) before starting the
  # next one; the shell is still in $WORK, so the new process is a direct child.
  kill "$FW_PID" 2>/dev/null
  for _ in $(seq 1 100); do kill -0 "$FW_PID" 2>/dev/null || break; sleep 0.2; done
  kill -9 "$FW_PID" 2>/dev/null; sleep 0.5
  local before
  before="$(grep -c 'Running Matter transport' "$WORK/firmware.log")"
  RUST_LOG="${FW_LOG:-info}" ./fusion-firmware >> firmware.log 2>&1 &
  FW_PID=$!
  for _ in $(seq 1 30); do
    [ "$(grep -c 'Running Matter transport' "$WORK/firmware.log")" -gt "$before" ] && return 0
    sleep 1
  done
  echo "FAIL  firmware did not come back after a restart"; return 1
}
topology() {
  (cd "$HERE" && node topology.mjs --storage "$WORK/ctl" --state "$WORK/topology.json" "$@" 2>&1 \
    | sed 's/\x1b\[[0-9;]*m//g' | grep -E '^(PASS|FAIL)')
  return "${PIPESTATUS[0]}"
}
bumps() { sed 's/\x1b\[[0-9;]*m//g' "$WORK/firmware.log" | grep -c 'ConfigurationVersion bumped'; }
TOPOLOGY_CODE=0
topology --mode record || TOPOLOGY_CODE=1
restart_firmware && topology --mode same || TOPOLOGY_CODE=1
echo "$([ "$(bumps)" -eq 0 ] && echo PASS || echo FAIL)  no ConfigurationVersion bump when nothing changed"
[ "$(bumps)" -eq 0 ] || TOPOLOGY_CODE=1
cat >> "$WORK/config/iiotedge_default.toml" <<'PROBE'

[[matter.endpoints]]
kind = "temperature_sensor"
name = "Topology probe"
source = "push:topology_probe"
endpoint = 99
PROBE
restart_firmware && topology --mode bumped --has 99 || TOPOLOGY_CODE=1
echo "$([ "$(bumps)" -eq 1 ] && echo PASS || echo FAIL)  the firmware logged the bump once"
[ "$(bumps)" -eq 1 ] || TOPOLOGY_CODE=1
restart_firmware && topology --mode same || TOPOLOGY_CODE=1
echo "$([ "$(bumps)" -eq 1 ] && echo PASS || echo FAIL)  and not again on the next boot"
[ "$(bumps)" -eq 1 ] || TOPOLOGY_CODE=1
[ "$TOPOLOGY_CODE" -eq 0 ] || CODE=$TOPOLOGY_CODE
set -e

echo "--- firmware ERROR/panic lines (excluding expected noise) ---"
# "Error reading attribute: Failure" is rs-matter logging a handler's deliberate answer
# for a NON-nullable attribute (BooleanState/Occupancy/Switch position) whose `push:`
# source has not been fed yet - the controller's first interview happens before the
# harness pushes values. "Error writing attribute: Failure" is the refused Sensitivity
# write controller.mjs makes on purpose. The strict attribute sweep covers real faults.
sed 's/\x1b\[[0-9;]*m//g' firmware.log \
  | grep -E 'ERROR|panick' \
  | grep -vE 'Telemetry engine|AI engine|UnsupportedAttribute|AttributeNotFound|ConstraintError|is synthetic \(mock|Error (reading|writing) attribute: Failure|Error invoking command: Failure' || echo "(none)"
echo "firmware log kept at $WORK/firmware.log"

# The demo endpoint set (demo-endpoints.toml, what a bench board is loaded with) must
# stay valid too. Only for the default full run; frees the Matter port first.
if [ -z "${MATTER_EXTRA_TOML+x}" ] && [ -z "${MATTER_SKIP_DEMO:-}" ]; then
  cleanup
  "$HERE/demo-check.sh" || CODE=1
  # Real attestation files and a custom setup code, end to end (attestation-check.sh).
  "$HERE/attestation-check.sh" || CODE=1
fi
exit "$CODE"
