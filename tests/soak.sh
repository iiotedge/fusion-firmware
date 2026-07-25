#!/usr/bin/env bash
# tests/soak.sh — SOAK / endurance test: run the real firmware unattended for
# hours and fail if resources trend upward (leaks) or the process dies. This
# is the "will it survive a week on the factory floor?" gate.
#
#   ./tests/soak.sh                 # default 1 hour
#   SOAK_HOURS=72 ./tests/soak.sh   # full release soak
#   SOAK_SAMPLE_S=30 ./tests/soak.sh
#
# Samples RSS / FD count / thread count / CPU every SOAK_SAMPLE_S into a CSV,
# then verdicts: RSS growth (leak), FD growth (handle leak), thread growth,
# any restart/panic. A flat RSS over hours is the pass condition.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

SOAK_HOURS="${SOAK_HOURS:-1}"
SOAK_SAMPLE_S="${SOAK_SAMPLE_S:-20}"
# Leak threshold: RSS must not grow more than this % from the settled baseline
# (measured after a warm-up) to the end.
RSS_GROWTH_MAX_PCT="${RSS_GROWTH_MAX_PCT:-15}"

build_release

WORKDIR="$(mktemp -d /tmp/iiotedge_soak.XXXXXX)"
CSV="$WORKDIR/soak.csv"
# Exercise the heavy paths: streaming + recording + tamper analytics, storage
# rotation churning (short chunks + tight cap so the janitor runs constantly).
make_test_config "$WORKDIR" '
[storage]
enabled = true
chunk_seconds = 15
max_total_mb = 200
[tamper]
enabled = true
[telemetry]
enabled = false
'

start_firmware "$WORKDIR" || exit 1

# Keep an RTSP client pulling the whole time so the encoder path stays hot.
( while kill -0 "$FW_PID" 2>/dev/null; do
    ffmpeg -v error -rtsp_transport tcp -i "$RTSP_URL_AUTH" -t 60 -f null - >/dev/null 2>&1 || sleep 2
  done ) &
PULLER_PID=$!

TOTAL_S=$(python3 -c "print(int($SOAK_HOURS*3600))")
END=$(( $(date +%s) + TOTAL_S ))
echo "elapsed_s,rss_kb,fds,threads,cpu_pct,frames_cap,frames_drop" > "$CSV"
log "Soaking for ${SOAK_HOURS}h (sample every ${SOAK_SAMPLE_S}s). CSV: $CSV"

START=$(date +%s)
RESTARTED=0
while [ "$(date +%s)" -lt "$END" ]; do
  if ! kill -0 "$FW_PID" 2>/dev/null; then
    fail "Firmware process DIED during soak (see $WORKDIR/firmware.log)"
    RESTARTED=1; break
  fi
  ELAPSED=$(( $(date +%s) - START ))
  RSS="$(rss_kb "$FW_PID")"; FDS="$(fd_count "$FW_PID")"
  THR="$(thread_count "$FW_PID")"; CPU="$(ps -o %cpu= -p "$FW_PID" 2>/dev/null | tr -d ' ')"
  FC="$(metric iiotedge_frames_captured_total)"; FD_="$(metric iiotedge_frames_dropped_total)"
  echo "${ELAPSED},${RSS:-0},${FDS:-0},${THR:-0},${CPU:-0},${FC:-0},${FD_:-0}" >> "$CSV"
  printf "\r  t=%5ds  rss=%6dMB  fds=%3s  thr=%3s  cpu=%5s%%  " \
    "$ELAPSED" "$(( ${RSS:-0} / 1024 ))" "${FDS:-0}" "${THR:-0}" "${CPU:-0}"
  sleep "$SOAK_SAMPLE_S"
done
echo

kill "$PULLER_PID" 2>/dev/null || true
# Precise signatures only: a real Rust panic ("thread '...' panicked at",
# Rust's own default panic-hook format) or the supervisor's own "a worker
# thread died" message. NOT a bare substring match on "panic" — several
# EdgeError variants are named things like AiPanic (e.g. "AI Engine Panic:
# ai.runtime 'rknn' ... Linux-only") for an expected, gracefully-handled
# condition (no RKNN on a dev host), not an actual crash.
PANIC_LINES="$(grep -E "thread '.*' \([0-9]+\) panicked at|CRITICAL FAULT" "$WORKDIR/firmware.log" || true)"
if [ -n "$PANIC_LINES" ]; then
  fail "Firmware log contains a panic / critical fault"
  echo "$PANIC_LINES" | sed 's/^/    /'
fi

# --- verdict: compare settled baseline (skip first ~10% warm-up) to tail ----
python3 - "$CSV" "$RSS_GROWTH_MAX_PCT" <<'PY'
import sys, csv
rows = list(csv.DictReader(open(sys.argv[1])))
thresh = float(sys.argv[2])
if len(rows) < 4:
    print("  ! too few samples to judge a trend"); sys.exit(0)
warm = max(1, len(rows) // 10)
base = rows[warm]
tail = rows[-1]
def grow(key):
    b, t = float(base[key]), float(tail[key])
    return t - b, (100.0 * (t - b) / b if b else 0.0)
rss_d, rss_pct = grow("rss_kb")
fd_d, _ = grow("fds")
thr_d, _ = grow("threads")
print(f"  baseline(t={base['elapsed_s']}s) RSS={int(float(base['rss_kb'])//1024)}MB "
      f"fds={base['fds']} thr={base['threads']}")
print(f"  final   (t={tail['elapsed_s']}s) RSS={int(float(tail['rss_kb'])//1024)}MB "
      f"fds={tail['fds']} thr={tail['threads']}")
print(f"  RSS growth: {rss_pct:+.1f}%  FD growth: {fd_d:+.0f}  thread growth: {thr_d:+.0f}")
ok = True
if rss_pct > thresh:
    print(f"  \033[31m✗ RSS grew {rss_pct:.1f}% (> {thresh}%) — possible memory leak\033[0m"); ok = False
if fd_d > 5:
    print(f"  \033[31m✗ FD count grew by {fd_d:.0f} — possible handle leak\033[0m"); ok = False
if thr_d > 2:
    print(f"  \033[31m✗ thread count grew by {thr_d:.0f} — possible thread leak\033[0m"); ok = False
if ok:
    print("  \033[32m✓ resources flat — no leak detected\033[0m")
sys.exit(0 if ok else 1)
PY
VERDICT=$?

stop_firmware
cp "$CSV" "$REPO_ROOT/soak-$(date -u +%Y%m%dT%H%M%SZ).csv" 2>/dev/null || true
log "Soak CSV saved to repo root."
if [ "$RESTARTED" -eq 0 ] && [ "$VERDICT" -eq 0 ] && [ "$FAIL_COUNT" -eq 0 ]; then
  rm -rf "$WORKDIR"
else
  log "Sandbox preserved for debugging: $WORKDIR"
fi
[ "$RESTARTED" -eq 0 ] && [ "$VERDICT" -eq 0 ] && [ "$FAIL_COUNT" -eq 0 ]
