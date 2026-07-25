#!/usr/bin/env bash
# tests/benchmark.sh — production BENCHMARK: measure the numbers a regression
# is judged against. Boots the real firmware on the mock camera and records
# latency / throughput / resource baselines, writing a machine-readable line
# to BENCHMARKS.md.
#
#   ./tests/benchmark.sh          # build release + measure + append to BENCHMARKS.md
#
# NOTE: mock-camera numbers on a dev host are a RELATIVE baseline (catch
# regressions), not the absolute production figure — real per-board numbers
# come from tests/live-hw.sh on the Radxa. The row records host + arch so the
# two are never confused.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

build_release

WORKDIR="$(mktemp -d /tmp/iiotedge_bench.XXXXXX)"
make_test_config "$WORKDIR" '
[storage]
enabled = true
chunk_seconds = 10
[telemetry]
enabled = false
'

log "Booting and timing time-to-ready…"
BOOT_START="$(python3 -c 'import time;print(time.time())')"
start_firmware "$WORKDIR" || exit 1
BOOT_END="$(python3 -c 'import time;print(time.time())')"
READY_MS="$(python3 -c "print(int(($BOOT_END-$BOOT_START)*1000))")"

# --- RTSP first-frame latency (connect → first decoded frame) --------------
log "Measuring RTSP first-frame latency…"
FF_START="$(python3 -c 'import time;print(time.time())')"
ffmpeg -v error -rtsp_transport tcp -i "$RTSP_URL_AUTH" -frames:v 1 -f null - >/dev/null 2>&1
FF_END="$(python3 -c 'import time;print(time.time())')"
FIRST_FRAME_MS="$(python3 -c "print(int(($FF_END-$FF_START)*1000))")"

# --- sustained FPS + bitrate over a 10s pull -------------------------------
log "Measuring sustained FPS/bitrate over 10s…"
STATS="$(ffmpeg -v error -rtsp_transport tcp -i "$RTSP_URL_AUTH" -t 10 -f null - 2>&1 \
  | tail -1)"
# ffmpeg prints 'frame= N fps= F ...' on the progress line to stderr; pull it
# from a -stats run instead for reliability.
FPS="$(ffmpeg -rtsp_transport tcp -i "$RTSP_URL_AUTH" -t 10 -f null - 2>&1 \
  | grep -oE 'fps=[ ]*[0-9.]+' | tail -1 | grep -oE '[0-9.]+')"
FPS="${FPS:-0}"

# --- resource footprint under steady stream+record -------------------------
log "Sampling steady-state RSS/CPU (10s)…"
# Warm up, then sample RSS a few times and average; grab CPU% via ps.
sleep 3
RSS_SUM=0; SAMPLES=5
for _ in $(seq $SAMPLES); do
  R="$(rss_kb "$FW_PID")"; RSS_SUM=$((RSS_SUM + ${R:-0})); sleep 1
done
RSS_MB=$(( RSS_SUM / SAMPLES / 1024 ))
CPU_PCT="$(ps -o %cpu= -p "$FW_PID" 2>/dev/null | tr -d ' ')"
THREADS="$(thread_count "$FW_PID")"
FDS="$(fd_count "$FW_PID")"
FRAMES_CAP="$(metric iiotedge_frames_captured_total)"
FRAMES_DROP="$(metric iiotedge_frames_dropped_total)"

# --- chunk finalize time (SIGTERM → playable file) -------------------------
log "Measuring chunk finalize on shutdown…"
FIN_START="$(python3 -c 'import time;print(time.time())')"
stop_firmware
FIN_END="$(python3 -c 'import time;print(time.time())')"
FINALIZE_MS="$(python3 -c "print(int(($FIN_END-$FIN_START)*1000))")"

HOST="$(uname -s)-$(uname -m)"
GIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo nogit)"
DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo
log "${BOLD}Benchmark results${RESET} (mock camera, $HOST)"
printf "  %-28s %s\n" "time-to-RTSP-ready"     "${READY_MS} ms"
printf "  %-28s %s\n" "RTSP first-frame"        "${FIRST_FRAME_MS} ms"
printf "  %-28s %s\n" "sustained FPS"           "${FPS}"
printf "  %-28s %s\n" "steady RSS"              "${RSS_MB} MB"
printf "  %-28s %s\n" "steady CPU"              "${CPU_PCT}%"
printf "  %-28s %s\n" "threads / fds"           "${THREADS} / ${FDS}"
printf "  %-28s %s\n" "frames captured/dropped" "${FRAMES_CAP} / ${FRAMES_DROP}"
printf "  %-28s %s\n" "chunk finalize (SIGTERM)" "${FINALIZE_MS} ms"

BENCH_MD="$REPO_ROOT/BENCHMARKS.md"
if [ ! -f "$BENCH_MD" ]; then
  cat > "$BENCH_MD" <<'HDR'
# Benchmarks

Production baselines captured by `tests/benchmark.sh`. Mock-camera rows are a
**relative** regression baseline on a dev host; on-device rows (from
`tests/live-hw.sh`) are the real per-board figures. Newest first.

| date (UTC) | git | host | ready ms | 1st-frame ms | fps | RSS MB | CPU % | thr/fd | frames cap/drop | finalize ms |
|---|---|---|---|---|---|---|---|---|---|---|
HDR
fi
# Insert the new row directly under the table header (keep newest first).
ROW="| $DATE | $GIT | $HOST | $READY_MS | $FIRST_FRAME_MS | $FPS | $RSS_MB | ${CPU_PCT} | $THREADS/$FDS | $FRAMES_CAP/$FRAMES_DROP | $FINALIZE_MS |"
python3 - "$BENCH_MD" "$ROW" <<'PY'
import sys
path, row = sys.argv[1], sys.argv[2]
lines = open(path).read().splitlines()
# Find the header separator row (|---|...) and insert right after it.
for i, ln in enumerate(lines):
    if ln.startswith("|---") or ln.startswith("| ---"):
        lines.insert(i + 1, row); break
else:
    lines.append(row)
open(path, "w").write("\n".join(lines) + "\n")
PY

log "Appended to BENCHMARKS.md"
rm -rf "$WORKDIR"
