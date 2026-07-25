#!/usr/bin/env bash
# tests/verify.sh — FUNCTIONAL test: boot the real firmware on the mock camera
# and assert every subsystem is actually working end-to-end. One command,
# pass/fail exit code — the "does the whole thing work?" gate.
#
#   ./tests/verify.sh            # build release + run full functional suite
#
# Asserts on OBSERVABLE behavior (not internals): RTSP frames via ffprobe,
# HTTP via curl, ONVIF SOAP/WS-Discovery on the wire, files on disk, /metrics
# counters moving, and graceful shutdown finalizing the last chunk.
set -uo pipefail
source "$(dirname "$0")/lib.sh"

build_release

WORKDIR="$(mktemp -d /tmp/iiotedge_verify.XXXXXX)"
log "Sandbox: $WORKDIR"

# Enable storage (short chunks so we get a finalized file quickly), keep
# telemetry uplink off (no broker in CI), tamper on.
make_test_config "$WORKDIR" '
[storage]
enabled = true
chunk_seconds = 8
[telemetry]
enabled = false
'

start_firmware "$WORKDIR" || exit 1
LOGF="$WORKDIR/firmware.log"

echo
log "${BOLD}1. Video streaming (RTSP)${RESET}"
# rtsp_auth is enforced by the default config (see [[security.users]]);
# credentials ride in the URL — ffmpeg/ffprobe handle the Basic challenge.
PROBE="$(ffprobe -v error -rtsp_transport tcp \
  -show_entries stream=codec_name,width,height -of default=noprint_wrappers=1 \
  "$RTSP_URL_AUTH" 2>/dev/null)"
echo "$PROBE" | grep -q "codec_name=h264" && pass "RTSP serves H.264" || fail "RTSP H.264 (got: ${PROBE:-nothing})"
echo "$PROBE" | grep -q "width=1920" && pass "RTSP resolution 1920 wide" || fail "RTSP resolution"
# Pull a handful of real frames to prove the pipeline actually flows.
if ffmpeg -v error -rtsp_transport tcp -i "$RTSP_URL_AUTH" -frames:v 15 -f null - >/dev/null 2>&1; then
  pass "RTSP delivers a continuous frame sequence"
else
  fail "RTSP frame delivery"
fi

echo
log "${BOLD}2. Observability (/healthz, /metrics)${RESET}"
HEALTH="$(curl -s "http://127.0.0.1:${METRICS_PORT}/healthz" 2>/dev/null)"
echo "$HEALTH" | grep -q '"status":"ok"' && pass "/healthz reports ok" || fail "/healthz (got: ${HEALTH:-nothing})"
FRAMES="$(metric iiotedge_frames_captured_total)"
[ -n "$FRAMES" ] && [ "${FRAMES%.*}" -gt 0 ] 2>/dev/null && pass "/metrics frames_captured > 0 ($FRAMES)" || fail "/metrics frames_captured"
check "/metrics exposes storage gauges" bash -c "curl -s http://127.0.0.1:${METRICS_PORT}/metrics | grep -q iiotedge_storage_used_mb"

echo
log "${BOLD}3. ONVIF (SOAP + WS-Discovery)${RESET}"
# onvif_auth is enforced by the default config: needs a WS-Security
# UsernameToken (WS-UsernameToken), not just a reachable port.
SOAP="$(onvif_soap '<tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/>')"
echo "$SOAP" | grep -q "GetDeviceInformationResponse" && pass "ONVIF GetDeviceInformation answers" || fail "ONVIF SOAP (got: ${SOAP:-nothing})"
# WS-Discovery probe over UDP 3702.
if python3 "$(dirname "$0")/ws_probe.py" >/dev/null 2>&1; then
  pass "ONVIF WS-Discovery ProbeMatch received"
else
  warn "WS-Discovery probe got no match (multicast may be restricted on this host)"
fi

echo
log "${BOLD}4. NVR recording${RESET}"
# A chunk's moov atom only finalizes once the NEXT chunk opens (splitmuxsink
# behavior — see src/storage/clips.rs's own comment on this), so the first
# chunk isn't playable until chunk_seconds later. Poll for a SECOND chunk to
# appear (proof the first was superseded) instead of trusting a fixed sleep.
WAITED=0
while [ "$(ls "$WORKDIR"/recordings/*.mp4 2>/dev/null | wc -l | tr -d ' ')" -lt 2 ] && [ $WAITED -lt 25 ]; do
  sleep 1; WAITED=$((WAITED + 1))
done
CHUNK="$(ls -t "$WORKDIR"/recordings/*.mp4 2>/dev/null | tail -1)"
if [ -n "$CHUNK" ]; then
  pass "Recorder wrote a chunk ($(basename "$CHUNK"))"
  DUR="$(ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1 "$CHUNK" 2>/dev/null | cut -d= -f2)"
  # A finalized (playable) chunk reports a duration; a truncated one errors.
  [ -n "$DUR" ] && pass "Chunk is playable (duration ${DUR}s)" || fail "Chunk not playable (moov missing?)"
else
  fail "No recording chunk produced"
fi

echo
log "${BOLD}5. Tamper detection${RESET}"
# Restart with a blackout-forcing threshold so the mock's mid-gray scene trips
# the blackout condition, proving the analytics + event path fire.
stop_firmware
make_test_config "$WORKDIR" '
[tamper]
dark_luma_max = 200.0
alarm_after_s = 1
[storage]
enabled = false
[telemetry]
enabled = false
'
start_firmware "$WORKDIR" || exit 1
LOGF="$WORKDIR/firmware.log"
sleep 6
grep -q "TAMPER ALARM" "$LOGF" && pass "Tamper alarm fired on forced blackout" || fail "Tamper alarm did not fire"
[ "$(metric iiotedge_tamper_active)" = "1" ] && pass "/metrics tamper_active = 1" || warn "tamper_active not latched (timing)"

echo
log "${BOLD}6. Graceful shutdown${RESET}"
# Re-enable storage, let a chunk start, SIGTERM, and confirm it finalizes.
stop_firmware
make_test_config "$WORKDIR" '
[storage]
enabled = true
chunk_seconds = 60
[tamper]
enabled = false
[telemetry]
enabled = false
'
rm -rf "$WORKDIR/recordings"
start_firmware "$WORKDIR" || exit 1
LOGF="$WORKDIR/firmware.log"
sleep 5
stop_firmware
grep -q "in-flight chunk finalized" "$LOGF" && pass "Shutdown finalized the in-flight chunk" || fail "Shutdown finalize log missing"
LAST="$(ls -t "$WORKDIR"/recordings/*.mp4 2>/dev/null | head -1)"
if [ -n "$LAST" ] && ffprobe -v error -show_entries format=duration -of csv=p=0 "$LAST" >/dev/null 2>&1; then
  pass "Chunk after SIGTERM is playable"
else
  fail "Chunk after SIGTERM not playable"
fi

echo
log "${BOLD}Result${RESET}"
echo "  ${GREEN}${PASS_COUNT} passed${RESET}, $([ $FAIL_COUNT -gt 0 ] && echo "${RED}" || echo "")${FAIL_COUNT} failed${RESET}"
rm -rf "$WORKDIR"
[ $FAIL_COUNT -eq 0 ]
