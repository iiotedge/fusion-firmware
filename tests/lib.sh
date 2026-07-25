#!/usr/bin/env bash
# tests/lib.sh — shared helpers for the firmware test harness.
#
# These tests drive the REAL firmware binary on the mock camera and assert on
# observable outputs (RTSP frames via ffprobe, HTTP via curl, files on disk,
# /metrics counters) — the behavior unit tests can't reach. Sourced by
# verify.sh / benchmark.sh / soak.sh.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${BIN:-$REPO_ROOT/target/release/iiotedge-firmware}"

# Ports must match the config the firmware is launched with.
RTSP_PORT="${RTSP_PORT:-8554}"
RTSP_PATH="${RTSP_PATH:-/live}"
METRICS_PORT="${METRICS_PORT:-9100}"
ONVIF_PORT="${ONVIF_PORT:-8000}"
RTSP_URL="rtsp://127.0.0.1:${RTSP_PORT}${RTSP_PATH}"
# Matches [[security.users]] in config/iiotedge_default.toml — the shipped
# default config enforces rtsp_auth/onvif_auth, so tests need real
# credentials, not just a reachable port.
AUTH_USER="${AUTH_USER:-admin}"
AUTH_PASS="${AUTH_PASS:-8506}"
RTSP_URL_AUTH="rtsp://${AUTH_USER}:${AUTH_PASS}@127.0.0.1:${RTSP_PORT}${RTSP_PATH}"

# Colors (disabled when not a TTY).
if [ -t 1 ]; then
  RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BOLD=$'\033[1m'; RESET=$'\033[0m'
else
  RED=''; GREEN=''; YELLOW=''; BOLD=''; RESET=''
fi

PASS_COUNT=0
FAIL_COUNT=0
FW_PID=""
WORKDIR=""

log()  { echo "${BOLD}[$(date +%H:%M:%S)]${RESET} $*"; }
pass() { echo "  ${GREEN}✓${RESET} $*"; PASS_COUNT=$((PASS_COUNT + 1)); }
fail() { echo "  ${RED}✗${RESET} $*"; FAIL_COUNT=$((FAIL_COUNT + 1)); }
warn() { echo "  ${YELLOW}!${RESET} $*"; }

# check "description" test-command...   → pass/fail on exit status
check() {
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}

# On macOS dev hosts the Homebrew gst-plugin-scanner hangs; the firmware sets
# GST_REGISTRY_FORK=no itself, but exporting it here too is harmless.
export GST_REGISTRY_FORK=no

# build_release — ensure an optimized binary exists (tests measure release).
build_release() {
  log "Building release binary…"
  ( cd "$REPO_ROOT" && cargo build --release ) || { echo "build failed"; exit 1; }
}

# make_test_config <dir> — write a self-contained config into <dir>/config,
# isolated from the repo's config so tests never touch production settings.
# Extra TOML fragments passed as $2 override keys in the copied default
# config (e.g. to enable storage). Merged INTO existing tables rather than
# appended — the default config already defines [storage]/[tamper]/etc., and
# TOML rejects a duplicate table header, so a naive append breaks parsing
# the moment an override touches a section that already exists.
make_test_config() {
  local dir="$1"; local extra="${2:-}"
  mkdir -p "$dir/config"
  # Start from the shipped default so we exercise the real schema, then
  # override paths to stay inside the sandbox.
  cp "$REPO_ROOT/config/iiotedge_default.toml" "$dir/config/iiotedge_default.toml"
  cp "$REPO_ROOT/config/edge.toml" "$dir/config/edge.toml" 2>/dev/null || true
  [ -z "$extra" ] && return 0
  python3 - "$dir/config/iiotedge_default.toml" "$extra" <<'PY'
import re, sys, tomllib

path, extra = sys.argv[1], sys.argv[2]
overrides = tomllib.loads(extra)
text = open(path).read()

def toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, str):
        return '"' + v.replace('\\', '\\\\').replace('"', '\\"') + '"'
    raise TypeError(f"unsupported override value type: {type(v)}")

def merge_section(text, section, kv):
    header = f"[{section}]"
    pat = re.compile(
        r"(?m)^\[" + re.escape(section) + r"\]\s*\n"
        r"((?:(?!^\[).*\n?)*)"
    )
    m = pat.search(text)
    if not m:
        # Section absent from the default config: append as a new table.
        body = "".join(f"{k} = {toml_value(v)}\n" for k, v in kv.items())
        return text.rstrip("\n") + f"\n\n{header}\n{body}"
    body = m.group(1)
    for k, v in kv.items():
        line_pat = re.compile(rf"(?m)^{re.escape(k)}\s*=.*$")
        new_line = f"{k} = {toml_value(v)}"
        if line_pat.search(body):
            body = line_pat.sub(new_line, body, count=1)
        else:
            body = body.rstrip("\n") + f"\n{new_line}\n"
    return text[: m.start()] + header + "\n" + body + text[m.end() :]

for section, kv in overrides.items():
    if not isinstance(kv, dict):
        raise SystemExit(f"only [section] tables are supported in test overrides, got: {section}={kv!r}")
    text = merge_section(text, section, kv)

open(path, "w").write(text)
PY
}

# start_firmware <workdir> — launch the firmware with cwd=<workdir> so it
# reads <workdir>/config/... and writes recordings/ there. Returns once the
# RTSP endpoint is serving, or exits non-zero on timeout.
start_firmware() {
  WORKDIR="$1"
  local log_file="$WORKDIR/firmware.log"
  ( cd "$WORKDIR" && exec "$BIN" ) >"$log_file" 2>&1 &
  FW_PID=$!
  log "Firmware started (pid $FW_PID); waiting for RTSP…"
  local waited=0
  while [ $waited -lt 30 ]; do
    if ! kill -0 "$FW_PID" 2>/dev/null; then
      echo "${RED}Firmware exited during startup. Log tail:${RESET}"
      tail -20 "$log_file"
      return 1
    fi
    if grep -q "RTSP server ready" "$log_file" 2>/dev/null; then
      log "Firmware ready."
      return 0
    fi
    sleep 1; waited=$((waited + 1))
  done
  echo "${RED}Timed out waiting for RTSP ready. Log tail:${RESET}"; tail -20 "$log_file"
  return 1
}

# stop_firmware — SIGTERM and wait (exercises graceful shutdown).
stop_firmware() {
  [ -n "$FW_PID" ] || return 0
  kill -TERM "$FW_PID" 2>/dev/null
  local waited=0
  while kill -0 "$FW_PID" 2>/dev/null && [ $waited -lt 10 ]; do sleep 1; waited=$((waited + 1)); done
  kill -KILL "$FW_PID" 2>/dev/null || true
  wait "$FW_PID" 2>/dev/null || true
  FW_PID=""
}

# metric <name> — print the current value of a Prometheus gauge/counter.
metric() {
  curl -s "http://127.0.0.1:${METRICS_PORT}/metrics" 2>/dev/null | awk -v n="$1" '$1==n {print $2}'
}

# onvif_soap <body-xml> — POST a SOAP request to the device service with a
# WS-Security UsernameToken (PasswordText, matching AUTH_USER/AUTH_PASS —
# see src/security.rs::parse_ws_token), since the default config enforces
# onvif_auth. <body-xml> is the content of s:Body.
onvif_soap() {
  local body="$1"
  curl -s -X POST -H 'Content-Type: application/soap+xml' --data "$(cat <<XML
<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd">
<s:Header><wsse:Security><wsse:UsernameToken>
<wsse:Username>${AUTH_USER}</wsse:Username>
<wsse:Password Type="#PasswordText">${AUTH_PASS}</wsse:Password>
</wsse:UsernameToken></wsse:Security></s:Header>
<s:Body>${body}</s:Body>
</s:Envelope>
XML
)" "http://127.0.0.1:${ONVIF_PORT}/onvif/device_service" 2>/dev/null
}

# rss_kb <pid> — resident set size in KiB (portable across macOS/Linux ps).
rss_kb() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' '; }
# fd_count <pid> — open file descriptors.
fd_count() {
  if [ -d "/proc/$1/fd" ]; then ls "/proc/$1/fd" 2>/dev/null | wc -l | tr -d ' ';
  else lsof -p "$1" 2>/dev/null | wc -l | tr -d ' '; fi
}
# thread_count <pid>
thread_count() { ps -M "$1" 2>/dev/null | tail -n +2 | wc -l | tr -d ' '; }

cleanup() { stop_firmware; }
trap cleanup EXIT INT TERM
