# TODO — Road to Production

Feature spec: [docs/FEATURES.md](docs/FEATURES.md). Rule: never remove an existing
feature; refactor and extend. `iiotedge-lib` is consumed as a path dependency, never modified.

**Reference target: Radxa Zero 3E (Rockchip RK3566)** — deploy via `make deploy`
(`radxa@<ip>`). i.MX 8M Plus remains supported through the same config-driven
encoder/HAL layers. Priority order (owner decision 2026-07-11): RTSP + ONVIF +
configurable GStreamer first; then storage/telemetry/correlation phases.

## Phase 0 — Repair the baseline (blockers) ✅ 2026-07-11
- [x] Fix `config/iiotedge_default.toml`: duplicate `[camera]` table makes boot fail at parse
- [x] Fix `src/hal/generic_v4l2.rs`: remove nonexistent `FrameHandle::dummy()`, implement real mmap dequeue, fix imports (Linux target currently cannot compile)
- [x] Fix `src/hal/nxp_isp.rs`: missing imports + incomplete `VideoSource` impl
- [x] Delete dead commented-out duplicate blocks (`main.rs`, `config.rs`, `hal/mod.rs`, …)
- [x] `git init` + `.gitignore` for `target/`; verify `cargo build` on host and `cargo check --target aarch64-unknown-linux-gnu`
- [x] CI: fmt + clippy(-D warnings) + test + cross-check

## Phase RTSP/ONVIF wave — pulled forward ✅ 2026-07-11 (verified on macOS host: ffprobe, curl SOAP, UDP probe)
- [x] Config v1.1: `stream.codec` (h264/h265), `stream.encoder` auto-probe + explicit override,
      `encoder_params` escape hatch, per-codec candidate lists (Rockchip MPP → NXP VPU →
      V4L2 stateful → VideoToolbox → software), `[onvif]` section, validation, serde defaults
      for zero-brick config upgrades
- [x] Log level/format (JSON/TEXT) + queue capacity + AI/stream enable flags wired from config
- [x] HAL factory takes `CameraConfig`; V4L2 backend negotiates width/height/format/fps
      (`set_format`/`set_params`); `ROCKCHIP_RKISP` alias; mock camera honors config and renders
      a moving test pattern
- [x] `stream::encoder`: encoder planning module (element availability probe, per-element
      bitrate/GOP property mapping, low-latency tuning defaults) — all vendor knowledge confined here
- [x] `stream::rtsp_server`: gst-rtsp-server with shared media factory, appsrc feed with
      need-data/enough-data flow control, GLib main-loop thread
- [x] ONVIF WS-Discovery responder (UDP 3702 ProbeMatch)
- [x] ONVIF Device+Media SOAP services: GetSystemDateAndTime, GetDeviceInformation,
      GetCapabilities, GetServices, GetScopes, GetProfiles, GetVideoSources, GetStreamUri, GetHostname
- [x] RTSP authentication (basic/digest) + ONVIF WS-UsernameToken enforcement (2026-07-17)
- [ ] Sub-stream profiles (`/sub`) and per-profile encoder settings (Phase 3 remainder)
- [ ] Verify on Radxa Zero 3E hardware: `mpph264enc`/`mpph265enc` selection, rkisp NV12 capture

## Phase 1 — Core refactor & SDK integration (mostly ✅ 2026-07-11)
- [x] Add `iiotedge-core`, `iiotedge-storage`, `iiotedge-security`, `iiotedge-protocols` as path deps (use-only; features mqtt+serial+canbus)
- [x] Tokio runtime alongside real-time threads (`src/telemetry.rs` engine thread; non-blocking `Ingestor` bridge)
- [x] Telemetry live end-to-end: AI events + health → SQLite persist-first buffer → MQTT **GDE JSON** (verified on broker: `iiotedge/<group>/<node>/camera/<id>/ai_event` with capture timestamps)
- [x] Southbound machine drivers config-only via `config/edge.toml` (serial + CAN/J1939 + Modbus samples for Radxa)
- [x] NVR chunk recording (`src/storage.rs`): splitmuxsink MP4 chunks, timestamped names, size/age rotation janitor (verified: 60 s chunk plays back h264 1080p)
- [x] Structured JSON logging per config (`log_format`)
- [x] Graceful shutdown (2026-07-16): SIGTERM/SIGINT → capture loop exits, router channels
      close, media thread drops recorder (EOS finalizes the in-flight chunk's moov), threads joined
- [x] `core/metrics.rs` (2026-07-16): Prometheus registry + `/metrics` + `/healthz` on
      [system].metrics_port — frames captured/dropped, AI detections, tamper alarms/active, uptime
- [ ] Config v2 (F12): layered load (file → per-device overlay → `IIOTEDGE__*` env), `--check-config` CLI
- [x] **Wire [watchdog] config** (2026-07-17): heartbeat staleness watchdog (analytics/media
      workers) + max_consecutive_dropped_frames → exit(2); realistic 8s default
- [x] **System health metrics** (2026-07-17): SoC temp / CPU / memory / throttle gauges on
      /metrics + health beacon (src/health.rs; best-effort sysfs/procfs)

## Phase 2 — HAL v2 (F11)
- [ ] Extend `VideoSource`: capabilities, format negotiation, sensor controls, hotplug recovery, hw timestamps
- [x] **Registry-based camera factory** (2026-08-02): `register_source("VENDOR_X", ctor)` +
      `OnceLock<Mutex<HashMap>>` registry in `src/hal/mod.rs`, replaces the old hardcoded
      `match`; `mock`/`generic_v4l2`/`nxp_isp` registered today, new backends need only
      implement `VideoSource` and call `register_source()` once
- [ ] Real `generic_v4l2` backend: mmap capture works; DMABUF zero-copy export and
      config-driven controls (exposure/gain/auto_exposure ioctls) still outstanding
- [ ] `generic_usb` (UVC) backend; keep `mock_cam`; `nxp_isp` completed for i.MX8MP
- [ ] `rtsp_in` proxy backend (adopt existing IP cameras)
- [ ] HAL conformance test suite runnable against mock on CI
- [ ] **Multi-camera per node** (gap analysis 2026-07-17): N VideoSources on one device
      (dual CSI / CSI+USB) with per-camera pipelines, mounts (/cam0, /cam1) and analytics

## Phase 3 — Streaming (F1)
- [ ] Encoder abstraction + registry: `h264`/`h265` switchable per profile, hw (VPU/V4L2) with sw fallback (x264/x265)
- [ ] GStreamer appsrc pipeline w/ zero-copy DMABUF import where supported; PTS from capture timestamps
- [ ] RTSP server (`gstreamer-rtsp-server`): multi-profile mounts (`/main`, `/sub`), auth, TCP/UDP
- [ ] Stream profiles in config (resolution/fps/bitrate/GOP/codec per profile)
- [x] **Microphone support** (2026-07-16): config-driven capture (`[audio]` — auto/ALSA/any
      GStreamer source), Opus/AAC (probed), one capture fanned out to the RTSP stream (pay1)
      and recording chunks (verified: two-track RTSP session + two-track finalized chunk).
      Pending: ONVIF audio source declaration, per-device gain control
- [x] **PTZ support** (2026-08-02): `PtzDriver` trait + registry mirroring the camera
      HAL (`src/ptz/mod.rs`), Pelco-D over RS-485/RS-232 backend (`src/ptz/pelco_d.rs`,
      protocol-tested), ONVIF PTZ service (`src/onvif/ptz.rs` — ContinuousMove/Stop/
      SetPreset/GotoPreset/GetPresets, advertised in GetCapabilities/GetProfiles only
      when `[ptz].enabled`) and MQTT commands (`ptz_move`/`ptz_stop`/`ptz_preset`),
      both dispatching through one shared `PtzController` so ONVIF and MQTT control
      never race each other. Safety watchdog auto-stops a ContinuousMove that's never
      followed by Stop (`[ptz].move_timeout_s`). ONVIF PTZ passthrough (for rtsp_in
      proxies) is still pending — blocked on the rtsp_in HAL backend above, same
      registry so it's a new backend module later, not a rewrite. Patrol routes and
      PTZ-on-event (zone violation → preset) also not yet built.
- [x] **Cloud-push relay** (2026-07-19, live-verified 2026-07-25): `stream_start`/
      `stream_stop` commands (src/commands.rs) drive an in-process GStreamer
      pipeline (src/stream/relay.rs: rtspsrc ! depay ! parse ! rtspclientsink)
      that republishes the local RTSP feed to media-ingestion-service's
      MediaMTX. Off by default ([cloud_relay].enabled); the LAN-only RTSP
      server is unaffected either way. Real push+playback round trip verified
      against production MediaMTX (srv1267737): async pipeline-failure
      watchdog added (src/stream/relay.rs's bus watcher) after the first
      round found silent failures on error; MediaMTX's `hlsVariant: mpegts`
      (was `lowLatency`, iOS-incompatible) and `hlsCDNSecret` (its default
      cookie/redirect session handshake breaks non-browser HTTP clients)
      both fixed on the server side. Confirmed working end-to-end via curl
      and the mobile app.
- [x] **Auto-reconnect with backoff** (2026-08-06, `src/stream/relay.rs`):
      real-world WAN flapping (mobile app logs showed RTSP publish sessions
      dying anywhere from ~9s to ~70s in, surfacing to viewers as repeated
      HLS 404s/segment-cancel cycles) traced to two firmware-side gaps, not
      an app bug. (1) The relay never retried on its own — recovery
      required something external to notice the stream was dead and
      re-issue `stream_start` over MQTT, the single biggest latency cost.
      Fixed: `watch_and_reconnect` now rebuilds the pipeline in place with
      exponential backoff (1s → 2s → 4s → 8s, capped 15s; resets to the
      floor once a pipeline stays up ≥20s so one bad patch doesn't leave
      every later reconnect slow) instead of tearing down and stopping.
      Retries indefinitely — `stream_stop` (or a fresh `stream_start`) is
      what ends it, not a retry-count ceiling, matching this being a
      best-effort layer on top of local recording that's never at risk.
      (2) Both `rtspsrc`/`rtspclientsink` were relying on GStreamer's
      default 20s `tcp-timeout`, so a silently-dead connection (dropped NAT
      mapping, black-holed WAN link, no clean RST/FIN) sat unnoticed for
      20s before anything could even start reconnecting. Fixed: explicit
      `tcp-timeout=5000000` (5s) on both, verified against the real element
      properties (`gst-inspect-1.0`), not guessed. Explicitly NOT a fix for
      RTSP-over-TCP's inherent multi-round-trip handshake cost or HLS's
      segment-boundary latency floor on the playback side — those are
      transport-level limits; WebRTC (ingest via WHIP, MediaMTX already
      supports it) is the real fix for sub-second glass-to-glass, flagged
      as a separate, larger piece of work, not bundled into this fix.
- [x] **WebRTC (WHIP) relay mode** (2026-08-06, `src/stream/relay.rs`):
      the separate, larger piece of work flagged above, requested directly:
      "are we using raw TCP or UDP... do as other company follow like
      Hikvision... use webrtc if needed if industry follow." Confirmed the
      relay was TCP on both legs (rtspsrc pulling locally, rtspclientsink
      pushing to MediaMTX) — exactly the transport Hikvision/Verkada/Ring/
      Nest-tier live view avoids for internet viewing, for the TCP head-
      of-line-blocking reason already on record above. `stream_start`
      gained a `mode` field (`"rtsp"` default, unchanged behavior, or
      `"webrtc"`) routing through `whipclientsink` (gst-plugins-rs's
      `rswebrtc`, WHIP = WebRTC-HTTP Ingestion Protocol, what MediaMTX's
      WebRTC ingest speaks) instead of `rtspclientsink` — same `rtspsrc !
      depay ! parse` head, different tail, same auto-reconnect/backoff
      machinery (mode-agnostic by design). Does NOT replace RTSP relay
      mode — third-party VMS/ONVIF consumers still need RTSP; both modes
      coexist, chosen per `stream_start` call via `mode`, not a device-
      wide switch. `mode = "webrtc"` needs `whip_url` (WHIP POST endpoint)
      instead of `publish_url`; auth is WHIP's own standard `Authorization:
      Bearer <command_token>` header, not a URL-embedded credential like
      RTSP mode. New `[cloud_relay].stun_server`/`turn_server` (both
      optional; empty = `whipclientsink`'s own public-STUN default) for
      WHIP-mode NAT traversal. Every non-obvious piece of this — that
      `whipclientsink`'s `video_%u` pad accepts raw `video/x-h264` directly
      (no manual `rtph264pay` needed), and the `signaller::whip-endpoint`/
      `signaller::auth-token`/`turn-servers=<"...">` launch-string syntax —
      was verified against the real installed element (`gst-inspect-1.0`,
      `gst-launch-1.0` reaching PLAYING) before writing the Rust code, not
      guessed; one genuine bug (an extra, wrong layer of quote-escaping on
      `turn-servers` copied from a shell-quoting test) was caught this way
      before it shipped. Not yet live-verified against a real MediaMTX WHIP
      endpoint from a real device (no WHIP-capable MediaMTX instance
      reachable from this dev machine) — same category of gap as the
      original RTSP relay before its 2026-07-25 live verification; do that
      before calling this production-ready, the same way that was done.

## Phase 4 — Overlays (F6) (core ✅ 2026-07-16)
- [x] Overlay engine before encode: wall-clock timestamp (top-right), device id (top-left),
      custom text (bottom-left) via runtime-probed pango elements; config-driven font/format;
      identical burn-in on RTSP and NVR chunks (shared drawn buffer, fewer copies than before)
- [x] AI bounding boxes drawn into the luma plane with TTL expiry ([overlay] section
      supersedes stream.osd_overlay_enabled)
- [ ] Tamper banner overlay (with Phase 8), logo image, privacy masks
- [ ] Machine-data overlay fields bound to southbound tags (needs Phase 7 correlation)
- [x] **Realtime graph/chart overlays** (2026-07-16): `stream/widgets.rs` — sparkline trends,
      bar gauges and live values rendered into the frame (built-in bitmap font, dependency-free),
      bound to any telemetry source id via a lib-Processor tap; `[[overlay.widgets]]` layout
      config (verified visually over RTSP with live data). Pending: color/theming, more widget kinds
- [ ] Per-stream-profile overlay sets + hot-reload (needs Phase 3 sub-streams / Phase 12 config v2)

## Phase 5 — Local storage (F3) (core ✅ 2026-07-16)
- [x] Segmented circular recorder (MP4 chunks, retention by size+age; in-flight chunk finalized on shutdown)
- [x] Event↔evidence index (`events.jsonl`: event → chunk + snapshot) — SQLite upgrade later if query needs grow
- [x] Event clips with pre/post-roll: hardlinked chunks + JSON manifest in `<storage>/clips/`, per-reason debounce, own rotation cap (verified live)
- [x] JPEG snapshots on AI/tamper events (rate-limited, rotation-managed)
- [ ] Snapshot on demand (ONVIF GetSnapshotUri / command channel — Phases 6/10)
- [x] Storage health (2026-07-17): used/free gauges + min_free_mb watermark warnings
      (SNMP alerting joins Phase 12)
- [x] **SD-card export** (2026-07-17): auto mirror while media mounted + manual full sync
      (command channel `export` / GPIO button, debounced), fsync'd copies, per-target
      resume state (verified live: incremental mirror, playable copies, command roundtrip)
- [x] **Secure FTP export** (2026-07-17): FTPS (rustls) / plain FTP upload with remote
      mkdir -p layout, self-signed opt-in, delete-after-upload for chunks, retry-next-pass.
      Pending: verify against a real site FTP server; SFTP variant; bandwidth cap
- [x] **Recording schedules** (2026-07-17): weekly shift/calendar windows gate recording
      (storage.record_mode=schedule); src/schedule.rs, midnight-crossing supported
- [x] **Zone motion detection** (2026-07-17): luma-diff zones with debounce → motion_event +
      snapshot; gates recording (record_mode=motion/motion_and_schedule + post-roll); src/motion.rs

## Phase 6 — Telemetry & commands via iiotedge-lib (F8) (core ✅ 2026-07-16)
- [x] Boot SDK engine: `EngineBuilder` + `SqliteBuffer` + `MqttTransport` (GDE JSON verified live; Sparkplug B by config)
- [x] TLS/mTLS via `iiotedge_security::build_client_tls` (enable in edge.toml [northbound.tls])
- [x] Publish AI/tamper/correlation/health events as `UnifiedPayload` (persist-first)
- [x] Command channel (`src/commands.rs`): status / snapshot / clip / config_get / reboot
      over `iiotedge/<group>/<node>/cmd` with acks + audit log (verified live on the platform broker)
- [ ] Command channel v2: config_set + validation, model swap, log level, recording start/stop; command auth

## Phase 7 — Machine-data correlation (F7) (core ✅ 2026-07-16)
- [x] Southbound machine drivers via `iiotedge_protocols::build_drivers` (Modbus/serial/CAN from edge.toml)
- [x] Correlation engine (`src/correlation.rs`): Processor tap in the lib's ingest path,
      [correlation] rules (source prefix + payload substring), bounded hit channel
- [x] Correlated event output: {machine payload, machine ts, frame id+ts, delta_ms,
      within_tolerance, snapshot, clip} → GDE correlated_event + events.jsonl + evidence actions
- [ ] Verify with physical machine data on the Radxa (serial scanner / CAN adapter)
- [ ] Clock sync: PTP client w/ NTP fallback; sync status metric; timestamps carried capture→PTS→RTCP→index
- [ ] Machine values → overlay fields/graphs (Phase 4 realtime-graph item)

## Phase 8 — Tamper detection (F4) (analytics core ✅ 2026-07-16)
- [x] Analytics: blackout, blinding (brightness attack), occlusion/defocus, freeze,
      scene-change — luma-grid statistics with sustain/recover debouncing ([tamper] config);
      runs even when [ai] is disabled
- [x] Fan-out: GDE tamper_event via persist-first MQTT + thick warning border burned onto
      live RTSP and NVR chunks
- [ ] Physical: GPIO case-open, IMU move (hardware permitting)
- [ ] ONVIF Events (pull-point) + SNMP trap fan-out (Phases 10/12); event-triggered clip (Phase 5)

## Phase 9 — ML/AI production engine (F5) (core ✅ 2026-07-16)
- [x] **Runtime-agnostic architecture**: `ai::runtime::InferenceRuntime` trait + config-selected
      backends (`ai.runtime`). ONNX Runtime built in; rknn/tensorrt/openvino/hailo/tflite are
      recognized config values with guidance — each lands as one module in src/ai/backends/
      behind the same trait, zero pipeline changes (industry HW-accel roadmap)
- [x] Real `ort` session: honest delegate handling (CPU provider compiled; accelerator EPs are
      ort cargo features), model sha256+size fingerprint logged at load
- [x] **RKNN backend** (2026-07-16): Rockchip NPU via runtime dlopen of librknnrt.so
      (ai.runtime = "rknn"; .rknn model from the same ONNX export via rknn-toolkit2; runtime
      quantizes/dequantizes so the yolov8 parser is unchanged). Linux ort now load-dynamic —
      no static onnxruntime in the cross link. Pending: exercise on physical RK3566 NPU
- [x] YOLOv8/v9/v11 decode + per-class NMS in `ai/parser.rs` behind the pluggable
      `OutputParser` trait (`ai.parser`); handles both export layouts
- [x] Preprocessing: NV12/YUY2 sampled directly into letterboxed NCHW f32 (single pass),
      ROI window with full-frame coordinate mapping back
- [x] ROI, class filter, labels, confidence, NMS IoU, input size, intra-threads,
      inference-fps limiting — all from config with serde defaults
- [x] Graceful degradation: missing/incompatible model logs the reason and video
      streaming/recording continue (no crash-loop)
- [x] Unit tests (letterbox round-trip, YOLO decode both layouts, NMS, class filter);
      live-verified with real yolov8n.onnx (~50 ms/frame CPU on dev host)
- [x] RKNN backend (2026-07-16): librknnrt.so dlopen'd at runtime (no link-time vendor
      coupling — one fleet binary), FLOAT32/NCHW in with want_float dequant out, model
      sha256 + SDK version logged. Experimental until exercised on a physical NPU with a
      .rknn conversion of the same ONNX export
- [ ] Model registry (id+version), atomic hot-swap, OTA model delivery (needs Phase 6 command channel)
- [ ] Two-stage pipelines (detector → classifier crop) and RT-DETR/segmentation parsers

## Phase 10 — ONVIF & security (F2)
- [x] WS-Discovery responder (shipped 2026-07-11)
- [x] Device Management + Media services (profiles ↔ stream, URIs — shipped 2026-07-11)
- [ ] Events service (pull-point) wired to tamper/AI/correlation events
- [ ] ONVIF/RTSP shared user store; Profile T (media2/H.265, metadata stream) behind feature flag
- [x] **RTSP authentication** (2026-07-17): gst-rtsp-server Basic auth, viewer-role token
- [x] **ONVIF WS-UsernameToken enforcement** (2026-07-17): PasswordText + PasswordDigest,
      shared [[security.users]] store (src/security.rs)
- [x] **Command-channel TLS + auth** (2026-07-17): bearer token + edge.toml TLS (raw-PEM)
- [ ] **RTSP transport encryption** (RTSP-over-TLS / SRTP) for sites that require encrypted video

## Phase 10b — Local web admin UI (gap analysis 2026-07-17)
- [ ] Embedded management page on the metrics/ONVIF HTTP port: live snapshot view,
      health/metrics dashboard, event log browser (events.jsonl), config viewer with
      validated edit + apply, log tail — auth-gated, works offline (no CDN)

## Phase 11 — Cluster mode (F9) (core + build verified 2026-07-24)

Design note: `src/cluster/` is written against plain types behind a
`ClusterTransport` trait specifically so it can be lifted into `iiotedge-lib`
verbatim later (owner decision) — it lives in the firmware repo only until it
has proven itself in one deployment. Two independent, combinable transports
replace the originally-planned MQTT-broker cluster bus with something that
keeps working through a WAN/internet outage:

- [x] `ClusterTransport` trait + `src/cluster/mod.rs`: node registry, simplified
      deterministic bully election (highest (priority, node_id) among peers heard
      from within a timeout window — self-healing, no election message round-trip
      needed), `ClusterHandle` (publish_event / drain_peer_events), Prometheus
      `cluster_peer_count` / `cluster_is_leader` gauges
- [x] **WiFi/LAN transport** (`cluster/wifi.rs`): broker-less UDP multicast — no
      MQTT broker, no internet, works on an isolated site LAN or a camera-hosted
      AP. `[cluster.wifi]` config (multicast group/port/ttl)
- [x] **Bluetooth LE transport** (`cluster/bluetooth.rs`, Linux/BlueZ via `bluer`,
      cfg-gated): advertising-beacon design (broadcast + scan only, no GATT
      connections) for sites with no WiFi network reachable at all; small
      payload cap traded for a much smaller/more stable slice of the BlueZ API.
      `[cluster.bluetooth]` config
- [x] Cross-camera event reactions: `[[cluster.reactions]]` (match peer event
      kind → local snapshot), wired to the existing manual-snapshot evidence path
- [x] Local events (tamper/motion/ai/correlated) relay onto the cluster bus
      alongside their normal GDE publish
- [x] **Ping/Pong liveness probe**: periodic active probe (phase-offset from
      Announce, same jittered scheduling) so peer-down detection doesn't wait
      out a full Announce timeout window on a single lost datagram
- [x] **Cross-device agentic control** (rule-based, no LLM): `[[cluster.reactions]]`
      gained `remote_cmd`/`remote_target` — a matched peer event can tell another
      peer (or every peer, `"*"`) to run a command, not just react locally.
      Peer-issued commands run through the exact same `commands::handle_command`
      path (and bearer-token check) as MQTT ones — gated by the receiving
      device's `[cluster].accept_remote_commands` (default off)
- [x] **Collision avoidance**: `Jitter` (xorshift64, seeded from wall-clock +
      node_id, no new crate dependency) applies ±20% jitter to Announce/Ping
      broadcast intervals so same-config devices don't drift into lockstep —
      layered on top of the WiFi/BLE MAC-layer collision handling, not a
      replacement for it
- [x] **`/cluster/status` HTTP endpoint** (`core/metrics.rs`, alongside
      `/metrics`/`/healthz`): leader/peer-count/peer-list/recent-command-acks
      JSON snapshot — the mobile app integration point (Phase 11b)
- [x] Build-verified on host: `cargo build`/`cargo clippy -- -D warnings`/`cargo
      test` (59/59, `cluster::` covers election, peers/touch_peer, jitter bounds,
      MessageKind JSON round-trips) all clean
- [x] aarch64 cross-verify: `make docker-lint` (clippy -D warnings) and `make
      docker-release` both clean on target (Radxa Zero 3E's
      aarch64-unknown-linux-gnu)
- [ ] Live verify: two-instance WiFi multicast exchange + leader election is
      covered by `#[cfg(test)]` and passes; physical two-Linux-box BlueZ
      verification and a real two-device on-site Ping/Command exchange are
      still outstanding (simulation/unit-test coverage only so far)
- [ ] mDNS discovery as a third option / hybrid with the UDP announce beacon
- [ ] Distributed-AI hooks (multi-view detection fusion, task handoff) — Phase 11c

### Phase 11b — Mobile app integration point — scoped down 2026-07-24 to
### firmware-side export only; no changes made to the mobile-app repo
- [x] `GET /cluster/status` (`core/metrics.rs`, built in Phase 11 above) is the
      full integration surface: plain read-only HTTP/JSON on the existing
      `[system].metrics_port`, documented in `docs/FEATURES.md` F9. Any
      client — the mobile app, a dashboard, `curl` — can consume it without
      firmware-repo changes; building the actual mobile UI against it is
      mobile-repo work, out of scope here.

### Phase 11c — Cross-device AI (correlated detections + peer-triggered
### re-analysis + distributed inference) — correlated detections and
### peer-triggered re-analysis DONE and build-verified 2026-07-24;
### distributed inference (task handoff) is Phase E, still deferred
- [x] Cluster detection-fusion module `src/cluster/fusion.rs`
      (`DetectionFusion`, mirrors `correlation.rs`'s match + time-tolerance
      pairing pattern): correlates this device's own recent AI detections
      against peers' `ai_event` broadcasts (label only — no wire protocol
      change needed, reuses the existing compact per-batch summary) —
      matches within `[cluster.fusion].tolerance_ms` publish a
      `fused_detection` telemetry event. Off by default.
- [x] Peer-triggered re-analysis: new `reanalyze` command (`commands.rs` +
      `InferenceEngine::force_next()` in `ai/engine.rs`) bypasses
      `ai.inference_fps_limit`'s skip for one frame. Reachable over MQTT or
      — the actual cross-device path — a `[[cluster.reactions]] on_kind =
      "ai_event", remote_cmd = "reanalyze"` rule: no new plumbing beyond
      Phase A's existing remote-command mechanism, config-driven like
      everything else in `[[cluster.reactions]]`
- [ ] Actual distributed inference (Phase E, deferred): task handoff for a
      heavier model / overloaded-device offload — needs a load/capability
      signal in Announce first (`priority` alone isn't a capacity metric
      today)

## Phase 12 — Fleet & mass deployment (F10)
- [x] **Device identity** (2026-08-04): hw-derived `device_id` when
      `[system].device_id` is left empty — device-tree serial-number (survives
      an SD/eMMC re-flash) → primary NIC MAC → random UUID, persisted to
      `[system].identity_file` so it's stable across reboots (`src/identity.rs`).
      TPM-backed identity not built (no TPM assumed present on the reference
      hardware; device-tree serial already gives a hardware-anchored ID without one).
- [x] **Device footprint** (2026-08-04): `GET /footprint` (model, hardware_id,
      firmware version + build-time git hash via a new `build.rs`, sha256 of
      the exact config file booted with, enabled-feature list) plus a one-time
      GDE `device_birth` telemetry event at boot, mirroring Sparkplug's own
      NBIRTH (`src/footprint.rs`, `core/metrics.rs`)
- [x] **SNMP agent** (2026-08-04): v2c only (v3's USM auth/privacy is separate,
      real complexity — not built). Hand-rolled BER/ASN.1 (`src/snmp/ber.rs`) and
      GetRequest/GetNextRequest/Trap-v2c dispatch (`src/snmp/mod.rs`) — no mature
      Rust SNMP agent crate exists, and the wire format is small/fixed enough
      that hand-rolling was less risk than adopting a half-fit dependency.
      MIB-II system group + private enterprise MIB (streams/tamper/storage/
      telemetry-enabled) backed by the same `Metrics` counters `/metrics`
      already serves — one source of truth. Verified against **real
      `snmpget`/`snmpwalk` (net-snmp)**, not just this module's own encoder
      read back by its own decoder (`#[ignore]`d interop test, run manually —
      CI doesn't install net-snmp tooling). Enterprise OID `1.3.6.1.4.1.99999`
      is an obvious placeholder (IANA Private Enterprise Numbers is a real
      registry; this repo has no assigned number) — replace before pointing
      production NMS tooling at it. Tamper-alarm trap wired; storage-full
      trap not (no existing threshold-check site to hang it off yet).
- [x] **systemd `sd_notify` watchdog + packaging** (2026-08-04): `READY=1`
      once every worker is up, `WATCHDOG=1` pinged from inside the *same*
      liveness check that already drives the firmware's own exit(2)-on-stall
      (`core/sd_notify.rs`) — one source of truth for "healthy," and a real
      new failure mode caught (the main capture thread itself blocking
      forever, which the internal watchdog alone can't see since nothing
      else would notice). `deploy/fusion-firmware.service` updated
      (`Type=notify`, `WatchdogSec=30`). `.deb` packaging via `make deb`
      (`scripts/build-deb.sh`, hand-rolled with `dpkg-deb` rather than
      `cargo-deb` — no extra cargo plugin needed, `dpkg-deb` is already in
      Dockerfile.cross's Ubuntu base) — installs to `/opt/fusion-firmware`,
      runs as a dedicated non-root `fusion-firmware` system user, does
      **not** auto-start (shipped config still has `CHANGE-ME` secret
      placeholders). Yocto packaging **not built** — a full BSP layer/recipe
      needs a real Yocto build environment to develop against, not something
      to write blind; flagged, not attempted.

### Phase 12c — First-boot provisioning + certificate enrollment (DESIGN-ONLY,
### not started — no CA/provisioning server exists yet for this fleet,
### confirmed 2026-08-04, same "don't build infrastructure blind" call as 12d)
Goal: a device boots from a golden image with **zero shared secrets baked
in** and comes out the other side with a unique identity cert, unique
`command_token`/`api_token` (today's `CHANGE-ME` placeholders), and TLS
trust for the telemetry MQTT broker — without anyone typing per-device
secrets in by hand at flash time (that doesn't scale past a handful of units
and is exactly the "same secret across a fleet" anti-pattern the README's
"Before shipping a real device" note already warns about).
- [ ] **Provisioning server is a separate, new project** — not something to
      build from this firmware repo. Realistic options to evaluate there
      (not decided, just the real candidates): [step-ca](https://smallstep.com/certificates/)
      (mature, EST/ACME support, purpose-built for exactly this "fleet of
      IoT devices enrolls for a cert" problem) vs. a custom minimal REST
      enrollment endpoint (less capable, less to run, faster to stand up
      for a first fleet). Whichever it is, the firmware side below only
      needs it to speak ONE of: EST (RFC 7030), ACME, or a simple
      REST POST-CSR-get-cert-back contract — pick before writing firmware
      enrollment code, not after guessing
- [ ] **Bootstrap trust**: the device needs to trust the provisioning
      server on first contact, and the server needs to trust the device is
      a real, authorized unit and not an impersonator. Standard pattern:
      a manufacturing-time bootstrap credential baked into the golden image
      (a shared HMAC key or per-batch token, NOT a unique per-device secret
      — the whole point is the golden image stays generic) that's good for
      exactly one enrollment call and nothing else, expires/is revoked
      server-side after use
- [ ] Firmware-side flow (buildable once the server contract is picked):
      generate a device keypair on first boot (never leaves the device),
      build a CSR carrying the hw-derived `device_id` (src/identity.rs) as
      the cert's CN/SAN, POST it + the bootstrap credential to the
      provisioning server, receive back a signed cert + the real fleet CA
      chain, write both to disk, then generate and locally persist random
      `command_token`/`api_token`/`[ptz]`/`[snmp]` secrets that never leave
      the device (only the identity CERT is server-issued; the operational
      tokens are self-generated, since the server doesn't need to know them)
- [ ] Wire the issued cert into `[northbound.tls]` (edge.toml) for the MQTT
      telemetry uplink — the actual payoff: TLS mutual auth against the
      real fleet CA instead of the shared/no-TLS default this firmware
      ships with today
- [ ] Idempotency: a device that's already enrolled (cert + tokens present
      on disk) must skip this whole flow on every subsequent boot — this is
      a first-boot action, not a per-boot one
- [ ] Explicitly deferred, larger scope than the above: mTLS renewal
      before cert expiry (a whole re-enrollment flow of its own), and
      Phase 12b's QR-payload PAKE hardening — genuinely separate problems
      that happen to share "PKI" as a keyword, don't conflate them

### Phase 12d — OTA A/B updates (DESIGN-ONLY, not started — target OS
### image/bootloader/partition layout not yet decided, confirmed 2026-08-04)
Goal: a fleet device can be told (via the existing MQTT command channel,
matching every other remote-control surface this firmware already has) to
fetch and apply a signed firmware update, with a bad update rolling back
automatically instead of bricking the device — the entire reason A/B
(rather than in-place) update exists.
- [ ] **This blocks on a real decision this repo can't make alone**: does
      the fleet's actual OS image already have A/B-partitioned storage, or
      is that still to be designed? A stock Radxa/Armbian Debian image
      (the dev/test setup this whole engagement has used) has **no A/B
      partitioning by default** — adding it is a partition-table/bootloader
      change to the base OS image, not a firmware-binary change, and has to
      happen before any of the below is real rather than theoretical
  - [ ] If starting from scratch: [RAUC](https://rauc.io/) vs
        [swupdate](https://swupdate.org/) are the two mainstream Linux A/B
        update frameworks. RAUC leans toward a more opinionated, bundle-
        signed-as-one-file workflow (`.raucb`); swupdate is more flexible/
        scriptable but asks more of the integrator. Neither is clearly
        "right" without knowing the fleet's actual constraints (network
        bandwidth to devices, whether delta updates matter, existing
        Yocto/Debian tooling investment) — a real evaluation, not a coin flip
  - [ ] If a Yocto BSP already exists for this board: both RAUC and
        swupdate have mature meta-layers — the integration path differs
        significantly from a Debian/Armbian base, so this sub-decision is
        downstream of "what image are we actually shipping," not parallel to it
- [ ] Once a mechanism is picked, the **firmware-side** pieces this repo
      does own: an MQTT `ota_update` command (matching the existing v1 set's
      shape — `src/commands.rs`) that shells out to the chosen tool's CLI
      (`rauc install <bundle-url>` or the swupdate equivalent) rather than
      reimplementing bundle verification itself — signature checking is the
      update framework's job, this firmware shouldn't duplicate it
- [ ] Signed artifacts: whichever framework, the signing keypair is a
      release-infrastructure concern (where's the private key held, who's
      authorized to sign a release) — genuinely separate from anything in
      this repo, flagged so it isn't accidentally skipped when this phase
      is eventually picked back up
- [ ] Rollback: both RAUC and swupdate support boot-count-based automatic
      rollback (new slot fails to reach a "confirmed good" mark within N
      boots → bootloader reverts to the previous slot) — the firmware's own
      role is calling that framework's "mark this boot healthy" hook once
      it's confirmed its own worker threads came up clean (natural tie-in
      to the sd_notify `READY=1` call site, core/sd_notify.rs, added this session)
- [ ] Explicitly out of scope until the above is real: this firmware
      repo does not currently attempt any OTA logic, config migration
      across versions, or a rollback trigger — don't half-build this from
      assumptions about a partition layout that may not match reality

### Phase 12a — QR device onboarding — firmware side DONE and build/test
### verified 2026-07-24; mobile-app QR *scanner* is out of scope here (same
### "firmware-side export only" boundary as Phase 11b) — see docs/QR_ONBOARDING.md
- [x] `[security].api_token`: new bearer token, distinct trust tier from
      `command_token` — this is the credential the QR hands to the *mobile
      app*, so a compromised phone can never also issue commands
      (`src/config.rs`, `src/commands.rs` untouched)
- [x] `GET /cluster/status` now requires `Authorization: Bearer <api_token>`
      when `api_token` is set (empty ⇒ unauthenticated, same convention as
      every other auth knob in this firmware) — `core/metrics.rs`
- [x] `src/onboarding.rs`: builds the onboarding JSON payload (device_id,
      LAN host from the request's Host header, ONVIF/RTSP/metrics ports,
      RTSP+ONVIF credentials from `[security].users`, `api_token`) and
      renders it as a PNG QR code (`qrcode` + `image` crates, pure Rust,
      cross-compiles clean for aarch64)
- [x] `GET /onboarding/info` (JSON) and `GET /onboarding/qr.png` (PNG),
      served alongside `/metrics` on `[system].metrics_port` — gated by
      `[security].command_token` (bearer header or `?token=` query param,
      the latter so an installer can open the QR image directly in a
      browser); `[onboarding].enabled` (default true) can disable both
- [x] Boot-time posture warnings when `api_token`/`command_token` are empty
      while onboarding is enabled (`src/main.rs`, mirrors `security::log_posture`)
- [x] 5 unit tests (`src/onboarding.rs`): auth gating, payload shape
      (credentials embedded, gate secret never leaks into the payload),
      empty-users fallback, real PNG round-trip — `cargo test` clean
- [x] `docs/QR_ONBOARDING.md`: the mobile-app integration contract (payload
      schema, both endpoints, auth model, how to build the scanner) — no
      code changes made to the mobile-app repo, per the Phase 11b boundary
- [x] Tested for real on `iiotedge-cluster-sim`'s 3-node Docker cluster
      (real firmware, not a mimic) — auth gates, payload correctness, and
      the QR PNG all verified against the actual containers. This testing
      surfaced and fixed a real bug: the payload was reporting each
      process's *internal* listen port, not the externally-reachable one —
      invisible on a real single-camera LAN deployment (no NAT, so they're
      the same number) but very visible with 3 containers sharing one host
      behind Docker's port mapping. Fixed with `[onboarding].external_rtsp_port`/
      `external_onvif_port`/`external_metrics_port` overrides (`None` by
      default — a real deployment never needs to set these)
- [x] **Second real bug found on actual Radxa hardware** (2026-07-25, not
      the Docker test cluster): the payload never carried MQTT broker
      connection info or Sparkplug group_id/node_id — those live in a
      separate config document (`edge.toml`'s `EdgeConfig`), not this
      crate's own `[system]`. The mobile app's `qr_confirm_screen.dart`
      already worked around this by defaulting `mqttHost` to the QR's
      `host` field with an explicit comment flagging it as a guess — wrong
      whenever the broker isn't co-located with the camera, which is the
      normal case (confirmed on a real field deployment where the camera and
      MQTT broker were on entirely different hosts). Fixed in `src/onboarding.rs`:
      `OnboardingContext::from_config` now also loads `EdgeConfig` (when
      `[telemetry].enabled`) and the payload gained `group_id`, `node_id`,
      and `mqtt: {host, port, tls_enabled} | null`. 2 new tests
      (`payload_embeds_broker_and_node_identity_separately_from_device_id`,
      `payload_omits_mqtt_when_none_configured`) — 73/73 `cargo test` clean.
      `docs/QR_ONBOARDING.md` updated; **mobile app still needs to stop
      guessing `mqttHost` and read the new field instead — not done here,
      same firmware-only boundary as the rest of Phase 12a**

### Phase 12b — Onboarding hardening (future improvement, not started)
- [ ] Today's QR payload embeds RTSP/ONVIF passwords and `api_token` as
      **plaintext** — fine for a LAN-only camera whose ONVIF/RTSP already run
      on unencrypted transport, but a step below how the real industry
      standards do it: Matter (SPAKE2+ PASE) and Wi-Fi Easy Connect/DPP
      (ECDH) never put the long-term secret in the QR itself — only a
      public key or short setup code, with the real credentials derived
      over an encrypted session afterward. A screenshot/photo of today's QR
      is a permanent credential leak; a DPP/Matter-style QR is not.
- [ ] If/when this needs to be closed: QR carries a per-device public key
      (or short setup code) instead of raw credentials; onboarding adds a
      PAKE handshake (SPAKE2+ is the well-trodden choice — Matter uses it
      for exactly this) so `api_token`/RTSP-ONVIF credentials are minted
      into an encrypted session rather than transmitted in the clear.
      Meaningfully bigger lift than Phase 12a — new crypto dependency, a
      session-establishment protocol, and a matching change on the mobile
      app side — do not start without explicit sign-off.
- [ ] Smaller, standalone follow-ups worth doing regardless: TLS on the
      metrics/onboarding HTTP server (today it's plaintext like the rest of
      this firmware's LAN services), and a way to rotate `api_token` per
      mobile-app install (today it's one shared per-device secret, no
      per-app revocation) instead of only per-device rotation.

## Phase 13 — Hardening & release
- [ ] Integration test suite on mock HAL (full pipeline, event paths) in CI
- [ ] Soak test: 72 h run, zero leaks (RSS flat), restart storm test
- [ ] Security pass: ports audit, auth everywhere, secrets handling, fuzz config parser
- [ ] Docs: CONFIG.md schema, OPERATIONS.md (runbook), MIB file, ONVIF conformance notes
- [ ] **Serial/UART console logging** (gap analysis 2026-07-17): mirror `tracing`
      output to a serial device (`[system].console_uart = "/dev/ttyS0"`, baud) for
      headless field debugging when there's no network — early-boot + panic logs
      to the UART, standard on industrial edge devices. Also: kernel console on
      the same UART (device-tree/bootargs), documented in OPERATIONS.md

## Phase 13.5 — Test & verification tooling (gap analysis 2026-07-17)

The firmware's value lives in runtime behavior unit tests can't reach (RTSP
frames actually flowing, chunks finalizing, RSS flat over hours). So we ship a
test harness — a `tests/` toolkit that drives the real binary on the mock
camera and asserts on observable outputs (ffprobe, curl, /metrics, files).

- [x] `tests/verify.sh` — **functional test**: boot the firmware, assert every
      subsystem is actually working (RTSP pulls h264, /healthz + /metrics answer,
      ONVIF SOAP + WS-Discovery respond, chunk recorder writes a playable file,
      tamper alarm fires + clears, snapshot/clip/export commands round-trip,
      SIGTERM finalizes the last chunk). One command, pass/fail exit code.
- [x] `tests/benchmark.sh` + **BENCHMARKS.md**: measure and record production
      baselines — RTSP first-frame latency, sustained FPS/bitrate, per-subsystem
      RSS + CPU, event→telemetry latency, chunk finalize time, config load time.
      Establishes the numbers a regression is measured against.
- [x] `tests/soak.sh` — **soak/endurance test**: long unattended run
      (default 1 h, `SOAK_HOURS=72` for release) sampling RSS/FD/CPU/thread-count
      every N seconds; fails on RSS growth trend (leak), FD growth (handle leak),
      thread growth, or any panic/restart. Emits a CSV + summary verdict.
- [ ] Wire `tests/verify.sh` into GitHub CI (headless GStreamer on the runner)
- [ ] `tests/live-hw.sh` — on-device (Radxa) variant: same assertions against
      real rkisp capture + mpph264enc + real /dev/video0 (run over SSH from `make`)
- [ ] Config-parser fuzz target (`cargo-fuzz`) — malformed TOML must never panic

## Phase 14 — LiDAR & multi-sensor fusion (F14)

Same architecture discipline as everything else: a `PointSource` HAL trait +
registry (mirror of the camera's `VideoSource`), all vendor knowledge in one
backend module per device family, everything selected and tuned by config —
one firmware binary covers LiDAR-only nodes, camera-only nodes, and fusion
nodes.

### 14a — LiDAR HAL (config-only device swap)
- [ ] `PointSource` trait + factory: `initialize / start / next_cloud / stop`;
      `PointCloud { timestamp_ns, points[x,y,z,intensity,ring] }` + 2D `Scan` variant
- [ ] 2D scanner backends: SICK TiM/LMS (CoLa-A/B over TCP), Hokuyo URG/UST (SCIP2 serial/eth),
      Slamtec RPLidar (serial) — the cost-effective zone-guarding tier
- [ ] 3D LiDAR backends: Ouster OS0/OS1 (UDP + TCP config API), Velodyne VLP-16/32 (UDP packets),
      Livox Mid-360/HAP (SDK protocol), Robosense/Hesai — the AGV/volumetric tier
- [ ] Solid-state ToF rangefinders (Benewake TF, Terabee) via serial/I²C/CAN — the point-sensor tier
      (these can ride the existing iiotedge-lib serial/CAN drivers, config-only)
- [ ] Config: `[lidar]` type, transport (ip/port | serial device+baud), rotation/frame rate,
      FOV/range crop, mounting pose (x,y,z,roll,pitch,yaw) — the extrinsic reference frame
- [ ] Health: packet-loss / rotation-stall / dirty-window detection → tamper-style events

### 14b — LiDAR analytics (Industry 4.0 operations, no camera needed)
- [ ] Zone monitoring: config-defined 2D polygons / 3D boxes; presence/intrusion with
      sustain+recover state machines (same pattern as tamper) → GDE `zone_event`
      (assistive safety fields — SICK-style warning/protective zones; NOT a certified safety device)
- [ ] Min-distance / proximity measurement per zone → live value stream (feeds video widgets)
- [ ] Object clustering (euclidean) + size/centroid/velocity/trajectory estimation
- [ ] Counting: people/vehicle/pallet counting on gates and lanes (direction-aware)
- [ ] Level & volume measurement: silo/hopper/stockpile fill %, conveyor belt profile +
      volumetric throughput (m³/h) → GDE metrics
- [ ] Background learning + outlier/dust/rain filtering (industrial environments)
- [ ] Evidence: scan/cloud snapshots (downsampled) stored with events in the evidence index

### 14c — LiDAR + camera fusion
- [ ] Extrinsic calibration: LiDAR↔camera transform in config + guided calibration helper
      (board/corner-based); intrinsics from camera config
- [ ] Depth-on-video: project points onto frames — colorized depth overlay and per-region
      distance labels burned into RTSP + recordings (extends the widget/overlay engine)
- [ ] Cross-triggering both ways: LiDAR zone violation → camera snapshot/clip/PTZ-preset of that
      zone; camera event → LiDAR distance/position confirmation (reuses evidence actions)
- [ ] Fused evidence: correlated event carrying frame + cloud snippet + both timestamps
      (correlation engine already carries the timestamp-pairing machinery)

### 14d — LiDAR + camera + AI fusion
- [ ] 3D-localized detections: frustum-associate AI bounding boxes with clustered points →
      every detection gains distance, real-world size, position, velocity
- [ ] Multi-modal safety analytics: PPE/person detection × proximity zones
      (e.g. "person WITHOUT helmet within 2 m of press" → critical event + clip + overlay flash)
- [ ] Speed & collision analytics: forklift/AGV speed monitoring, person-vehicle closing-speed
      alarms, near-miss detection and statistics
- [ ] Volumetric anomaly detection: mis-stacked pallets, overhang detection, truck-fill optimization
- [ ] Occupancy heatmaps + dwell-time analytics (aggregated GDE metrics)
- [ ] Overlays: AI boxes annotated with fused distance/speed; zone states drawn on video
- [ ] Cluster mode tie-in (Phase 11): multi-camera + LiDAR fusion across nodes (future distributed AI)

### 14e — Use-case presets (config templates shipped in `config/presets/`)
- [ ] `safety-zone-guarding.toml` (2D scanner + camera verification)
- [ ] `silo-level.toml` (3D/ToF level + trend widget on video)
- [ ] `gate-counting.toml` (counting + ANPR-ready camera hooks)
- [ ] `forklift-safety.toml` (AI person/vehicle + proximity + speed)
- [x] **Camera+AI-only equivalent shipped 2026-08-04** (`config/presets/`,
      20 files, see `config/presets/README.md`): the 4 presets above are
      LiDAR-based and stay unbuilt/unchecked since Phase 14a-14d don't
      exist yet — a preset referencing `[lidar]` wouldn't actually run,
      so it couldn't honestly be called production-ready. Built the
      honest subset instead — 10 single-camera scenarios (perimeter
      intrusion, restricted machine safety zone, dock loitering, gate
      counting, production-line correlation+widgets HMI, multi-camera
      cluster mesh, after-hours lockdown, PPE compliance zone,
      cold-storage tamper monitoring, forklift/pedestrian shared lane),
      5 `cluster-fusion-*` multi-camera deployments covering
      target-customer verticals beyond factory-floor Industry 4.0
      (retail loss prevention, critical infrastructure/utility,
      campus/education, construction site, smart parking) built around
      cross-device detection fusion (`cluster/fusion.rs`), plus 5
      `home-*` residential smart-home scenarios (front door, driveway
      arrival→HA-lighting, garage, pool safety, whole-house mesh) built
      around Phase 19's Home Assistant/Zigbee integration — all built
      entirely on shipped features, every one exercised by
      `config::tests::every_shipped_preset_parses_and_validates`
      (`src/config.rs`) so they can't silently bit-rot. When Phase 14
      LiDAR ships, the 4 items above are still the right next step for
      true range-based presets (silo level, real proximity/speed) —
      this addition doesn't replace them.

## Phase 15 — Local LLM bridge & agentic control (F15) — STRICTLY ISOLATED, OFF BY DEFAULT

Design rule #1: the agent is a *separate, optional subsystem* — its own
`src/agent/` module behind BOTH a cargo feature (`--features agent`, can be
compiled out entirely) and a runtime `[agent] enabled` toggle. It talks to
the firmware ONLY through the same public surfaces a human operator has
(command channel, telemetry/events, config API) — zero private hooks into the
video/AI/storage paths, so switching it off changes nothing else.

### 15a — Local LLM bridge
- [ ] `[agent]` config: enabled=false, endpoint (OpenAI-compatible local runtimes:
      Ollama / llama.cpp server / vLLM on a LAN edge box), model, context budget,
      temperature; NO cloud dependency — local/LAN inference only
- [ ] Bridge client with health probe (agent auto-disables with a log when the LLM
      endpoint is down; firmware unaffected)
- [ ] On-device SLM path later (NPU-quantized small models) behind the same bridge trait

### 15b — Agentic tool layer (capability-gated)
- [ ] Tool registry mapping ONLY to existing operator surfaces: status, snapshot, clip,
      config_get/config_set(validated), camera controls, LiDAR zone queries (Phase 14),
      evidence-index search, metrics read, reboot
- [ ] Per-tool **capability allowlist in config** — the agent can call exactly what the
      site enables, nothing else; deny-by-default
- [ ] Hard interlocks regardless of allowlist: the agent can never disable tamper
      detection, recording, or telemetry; destructive tools require explicit opt-in
- [ ] Rate limits + dry-run mode (agent proposes, human approves via command channel)
- [ ] Full audit trail: every prompt, decision and tool call into the evidence index +
      GDE `agent_action` events

### 15c — Agentic behaviors (all config-declared)
- [ ] Event-driven reasoning: subscribe to AI/tamper/correlation/zone events; the agent
      decides follow-up actions within its allowlist (e.g. tamper alarm → snapshot burst,
      clip, notify with a written incident note)
- [ ] Natural-language operations: ask the camera questions over the command channel
      ("what happened on line 1 in the last hour?") — answers grounded in the evidence
      index, event log and metrics
- [ ] Incident summarization: draft structured reports from events.jsonl + snapshots
      (attach to GDE events for the platform)
- [ ] Scheduled patrols: periodic self-checks (stream health, storage, sensor health)
      with anomaly write-ups
- [ ] Cluster tie-in (Phase 11): agent reasoning over multi-node events (one agent per
      site coordinating cameras/sensors via the cluster bus)

### 15d — Safety & operations
- [ ] Kill switch: `[agent] enabled=false`, `cmd=agent_off` on the command channel, and
      the cargo feature for fleets that must not ship it at all
- [ ] Prompt-injection hardening: event/payload text treated as data, never as
      instructions; tool schemas strictly typed
- [ ] Resource budget: agent runs at lowest priority; LLM calls off-device by default —
      the video/AI/storage real-time paths are untouchable

## Phase 16 — Customizable AI detection rules (zones / line-crossing / loitering /
## workflow actions) — 16a/16b/16c DONE 2026-08-04, design entry 2026-07-24,
## requested by Santosh: "add a feature to customize object detection or any
## type of fencing... full flexibility in AI... like workflow"

Industry survey done before writing this entry (Axis Object Analytics user
manual, Frigate NVR's zones/masks/object-filters docs, ONVIF Profile M spec)
— every mainstream VMS/NVR analytics product (Axis, Hikvision, Dahua,
UniFi Protect, Frigate) converges on the same small vocabulary, so this
phase adopts it rather than inventing new terms:
- **Line crossing** ("virtual tripwire"): a 2-point line + direction
  (`a_to_b` / `b_to_a` / `either`); Axis also supports a "tailgating"
  window (>1 object crossing within N seconds counts as one alarm, not one
  per object) — worth adopting, cheap to add once crossing detection exists.
- **Intrusion / motion-in-area**: object present inside a polygon zone.
- **Loitering**: object remains inside a zone longer than a dwell threshold.
- **Object filters**: per-rule class allowlist + confidence override +
  min/max bounding-box size (suppresses both distant/tiny false positives
  and near-lens/huge ones) — Frigate calls the zone-gated version
  `required_zones`.
- Point-in-zone test convention (confirmed via Frigate docs — this is *the*
  standard, not just one implementation's choice): use the **bottom-center
  of the bounding box**, not its centroid — approximates the object's
  ground-contact point, which is what makes "inside the fenced area" mean
  the intuitive thing for a person/vehicle rather than triggering the
  instant the top of their bounding box clips the zone edge.
- **ONVIF Profile M** is the standardized wire format for exposing exactly
  these events (object detection/classification, line-crossing, intrusion,
  counting) to *any* third-party VMS, not just this project's own mobile
  app/MQTT — flagged as a stretch goal (16e below), genuinely separate and
  larger scope (new SOAP analytics/event schema + pull-point subscription
  service) from the core rule engine; don't conflate the two.

Sources consulted: [Axis Object Analytics scenarios](https://www.axis.com/products/axis-object-analytics/scenarios),
[Axis Object Analytics user manual](https://help.axis.com/en-us/axis-object-analytics),
[Frigate zones config](https://docs.frigate.video/configuration/zones/),
[Frigate object filters config](https://docs.frigate.video/configuration/object_filters/),
[ONVIF Profile M spec v1.0](https://www.onvif.org/wp-content/uploads/2021/06/onvif-profile-m-specification-v1-0.pdf).

### 16a — Zone/rule data model (config-driven, mirrors existing patterns)
- [x] **Generalizes, does not replace,** `motion.zones` (`config.rs`
      `MotionZone` — normalized `[0,1]` **rectangles only**, luma-diff
      based, stays exactly as-is: cheaper, non-AI, different trigger). New
      `[[ai.rules]]` array-of-tables (`AiRule` in `config.rs`), each rule:
      `name`, `enabled`, `classes` (subset of `ai.labels`, empty = any),
      `min_confidence` (optional override of `ai.confidence_threshold`),
      `zone` (list of normalized `[x,y]` points — 2 points = line, 3+ =
      polygon, so line-crossing and intrusion share one geometry field
      instead of two config shapes), `mode` (`"presence"` | `"line_cross"` |
      `"loiter"`), `direction` (line_cross only), `dwell_s` (loiter only).
      Documented example block in `config/iiotedge_default.toml`.
- [x] Per-rule `schedule` — reuses `ScheduleConfig`'s exact day/time-window
      pattern (`schedule.rs`) instead of a new scheduling format, so a rule
      can be "person in zone A, but only 10pm–6am"
- [x] Config validation at boot (mirrors the existing `ai.roi` length check
      in `config.rs`): rejects wrong point-count-for-mode, out-of-range
      `[0,1]` coordinates, missing/invalid `direction` on `line_cross`,
      `dwell_s == 0` on `loiter`, unknown class names (only when
      `ai.labels` is non-empty), and action-specific requirements
      (`webhook` needs `webhook_url`, `gpio_output` needs `gpio_chip`) —
      actionable error at boot, not a silent no-op rule. Self-intersecting
      polygon rejection not built (ray-casting point-in-polygon tolerates
      self-intersection well enough in practice; flagged, not attempted).

### 16b — Rule evaluation engine
- [x] New `src/ai/rules.rs` — post-processing layer between the parser
      (`ai/parser.rs`, untouched — still just decodes raw model output) and
      wherever detections currently get published, mirroring the existing
      separation where `tamper.rs`/`motion.rs` are independent analyzers,
      not changes to the shared pipeline
- [x] Point-in-polygon (ray casting) + point-side-of-line tests against each
      detection's bottom-center point (see convention note above)
- [x] Loiter tracking needs object persistence across frames (a rule fires
      once dwell_s is exceeded, not once per frame) — the AI engine has no
      tracker today (parser.rs is single-frame NMS only); this is the
      one genuinely new piece of state, not just config plumbing. Built as
      per-rule "seen since" timestamp keyed by **class alone** (not class +
      position bucket as originally planned — testing surfaced that keying
      by position fragmented a single loitering object's dwell timer across
      buckets whenever it drifted within the zone; class-only keying trades
      "confuses multiple same-class objects in one zone" — an accepted,
      documented tradeoff — for correctly tracking one). Line-crossing state
      is separately bucketed by position **projected along the line's own
      direction**, not raw (x,y) — needed so an object's "which side was it
      on" state survives the crossing motion itself. Full multi-object
      tracking (SORT/ByteTrack-style ID assignment) not built — the simple
      approach didn't prove false-positive-prone enough to need it yet.
- [x] A rule match produces a `RuleEvent` (name, mode, class, confidence,
      bbox, actions) — feeds the SAME downstream paths genuine detections
      already use: GDE `rule_event` telemetry, cluster `ai_event` broadcast
      (so cross-device fusion also sees rule-gated events, not just raw
      detections), snapshot/clip triggers (`main.rs`).

### 16c — Actions (what a rule *does*, the "workflow" part of the ask)
- [x] Per-rule `actions = [...]`: `telemetry_event` (always implicit —
      every fired rule publishes `rule_event` telemetry regardless of the
      configured action list), `snapshot`, `clip` (reuses `ClipExtractor`,
      same pre/post-roll as today), `cluster_broadcast` (reuses
      `cluster/fusion.rs`'s existing broadcast path)
- [x] `webhook`: HTTP POST to a configured URL with the rule-event JSON —
      the standard "wire this into anything" integration hook every
      surveyed product has in some form (Frigate: MQTT+webhooks; Axis/
      Hikvision/Dahua: HTTP notification profiles). Built as
      `ai/actions.rs`'s `WebhookDispatcher` — bounded channel + one worker
      thread (mirrors `storage::clips::ClipExtractor`'s shape) using `ureq`
      (blocking client, since this fires from a plain worker thread, not
      the tokio runtime telemetry already owns) — a slow/unreachable
      endpoint drops the request rather than stalling the analytics thread.
- [x] `gpio_output`: pulses a configured GPIO line (siren/relay/light) for
      `gpio_pulse_ms` — `gpio-cdev` was already a dependency, reused rather
      than adding a new one. Linux-only (`#[cfg(target_os = "linux")]`,
      warns and no-ops elsewhere), verified compiling for real via the
      aarch64 Docker cross-check (macOS `cargo check` only type-checks the
      non-Linux stub branch).
- [x] Explicitly OUT of scope for this phase: PTZ preset actions — no longer
      blocked (PTZ control shipped 2026-08-02, see PTZ section), but wiring
      a `ptz_preset` rule action is a follow-up, not bundled into this phase.

### 16d — Mobile app boundary (do not build this from the firmware repo)
- [ ] Firmware's job stops at: accept a rule definition (config today;
      ideally a `config_set`-style command later, validated the same as
      16a, instead of requiring a full config file replace+reboot cycle)
      and evaluate it. Drawing zones/lines on the live preview is a **mobile-
      app-repo UI concern** — same boundary already drawn for Phase 11b
      (cluster mesh) and Phase 12a (QR onboarding) — not started, not
      authorized here, don't build it from this repo
- [ ] If/when the mobile side takes this on: the normalized `[0,1]`
      coordinate space (already used by `motion.zones` and `ai.roi`) is
      exactly what a "draw on the video preview" UI needs — it survives
      resolution changes for free, no firmware-side translation required

### 16e — ONVIF Profile M (stretch, separate scope — see note above)
- [ ] Publish `rule_event`s as ONVIF analytics events (pull-point
      subscription service) so any third-party ONVIF VMS can consume this
      firmware's line-crossing/intrusion events natively, not just this
      project's own mobile app/MQTT — genuinely larger scope than 16a-c,
      don't start without explicit sign-off

## Phase 17 — Radar sensing & cross-modal fusion (F16) — 17a (mock backend
## + registry)/17b (analytics)/17e (cluster fusion) SHIPPED 2026-08-09,
## requested 2026-08-02: "integrate radar sensors, work in cluster mode
## along with AI and other available features [since] we are connecting
## together — plan accordingly as per Industry 4.0". 17c/17d/17f remain
## design-only — see each sub-phase below for exactly why.

Same architecture discipline as Phase 14 (LiDAR): a HAL trait + registry
(config-only device swap), analytics that stand alone with no camera
present, then progressively richer fusion with camera/AI/LiDAR — and, new
in this phase, fusion made **cross-device** from day one rather than added
later, because that is specifically what was asked for here.

Why radar earns its own phase instead of folding into 14 (LiDAR): it is
optically blind-spot-immune — FMCW/mmWave radar sees through dust, fog,
smoke and total darkness where camera and LiDAR both degrade or fail, and
it gives velocity directly (Doppler), with no frame-differencing or
tracker needed. That combination is the standard sensing layer for
quarry/mine haul roads and perimeter security specifically (permanent
airborne dust defeats optical sensors) — directly relevant given this
firmware's iotmining cloud-relay integration. Industry 4.0 framing: this
phase is what makes sensing genuinely **interoperable** (one fused event
model, not per-sensor silos) and **decentralized** (fusion happens
peer-to-peer over the existing broker-less mesh, no central server
required) rather than buzzword-only — see 17e.

### 17a — Radar HAL (config-only device swap)
- [x] **`RadarSource` trait + registry shipped 2026-08-09** (`src/radar/mod.rs`),
      mirroring `VideoSource`/`src/hal/mod.rs`'s `register_source()` pattern
      exactly, as planned — a new backend is one `impl RadarSource` +
      one `register_source()` call, `create_radar()`'s dispatch logic never
      changes. `PointSource` (14a) doesn't exist yet (LiDAR is still fully
      design-only) so this mirrors the camera HAL directly instead.
- [ ] Real vendor backends (TI IWR6843/1843, Acconeer A121/XM125, Xandar
      Kardian, Continental ARS408, Smartmicro, Navtech) — **deliberately
      NOT built**, same "don't build infrastructure blind" call as Phase
      12c/12d: these are proprietary binary wire protocols (TI's TLV frame
      format, CAN message layouts, etc.) with no real hardware or captured
      traffic available here to verify a parser against. Writing one
      without that is exactly how the WHIP relay's `whipclientsink` bug
      happened — a "verified" claim that was never actually checked
      against a real element (see `src/stream/relay.rs`'s header comment
      for the full account, corrected 2026-08-08). `create_radar()`
      degrades gracefully (`None`, logged, radar-off) on an unregistered
      `[radar].type`, so a future backend module is a pure addition.
- [x] **Both output tiers modeled, shipped 2026-08-09** (`RadarPoint`
      point-tier, `RadarTrack` track-tier, `RadarDetections` enum picking
      one — `src/radar/mod.rs`), exactly as planned; velocity + RCS fields
      included on both.
- [x] **Config shipped 2026-08-09**: `[radar]` type/transport/baud_rate/
      range/fov + mounting pose (x,y,z,roll,pitch,yaw), `[[radar.zones]]`
      (`src/config.rs`).
- [x] **Health reporting shipped 2026-08-09**: `RadarHealth`
      {interference, blocked, saturated} on the trait, tamper-style, same
      pattern as planned — `is_degraded()` logged from the radar thread
      (`src/main.rs`). The shipped `mock` backend always reports clean
      (nothing to simulate failing yet); a real backend populates it for
      real.

### 17b — Radar analytics (standalone, no camera needed)
- [x] **Zone presence/intrusion shipped 2026-08-09** (`src/radar/analytics.rs`)
      — same sustain-free per-frame polygon test `ai/rules.rs`'s presence
      mode uses (radar zones don't need sustain+recover the way tamper.rs's
      continuous statistics do; a detection is either in the zone this
      frame or it isn't).
- [x] **Direct speed measurement shipped 2026-08-09** — no new code
      needed, exactly as planned: Doppler `velocity_mps` is already a
      field on every detection, surfaced on every `RadarEvent`.
- [x] **Directional counting shipped 2026-08-09** — implemented as
      `line_cross` mode with `direction`, the same primitive Phase 16's
      `ai.rules` gate-counting preset already uses (one event per
      crossing; a downstream webhook/telemetry consumer tallies direction-
      separated counts, no counter state kept in-firmware — a deliberate
      consistency choice, not a missing feature).
- [ ] Micro-Doppler classification (vibration/gait signature) —
      **deliberately NOT built, not just unstarted**: needs raw ADC/
      spectrogram access most point/track-tier radar output doesn't
      expose (`RadarPoint`/`RadarTrack`, what this module actually
      consumes, have already thrown that information away by the time it
      reaches software). Shipping a "classifier" that can't classify
      anything real from the data this firmware actually has would be
      worse than not building it — flagged honestly instead
      (`src/radar/analytics.rs`'s header comment).
- [x] **Background/clutter learning shipped 2026-08-09** — a detection
      held at near-zero velocity in the same position bucket for N
      consecutive frames (a static reflector — fence post, parked
      equipment) is suppressed from zone evaluation entirely
      (`RadarAnalyzer::is_clutter`).

### 17c — Radar + camera fusion (DESIGN-ONLY, not started — needs a real
### radar backend to calibrate extrinsics against, not just the mock)
- [ ] Extrinsic calibration: radar↔camera transform in config (same
      pattern as 14c, distinct calibration helper since radar's sparse
      output doesn't support the same board/corner method — velocity-based
      moving-target alignment instead)
- [ ] Cross-triggering both ways: radar zone/speed violation → camera
      snapshot/clip of that bearing; camera AI event → radar range/velocity
      confirmation (reuses the existing evidence-action machinery)
- [ ] **Visibility-adaptive sensor arbitration**: when a configured
      low-light/low-contrast heuristic (or an explicit day/night+weather
      schedule) indicates degraded camera conditions, radar becomes the
      primary detector and camera/AI drop to verification-only — the
      concrete reason this firmware benefits from radar specifically,
      not just "another sensor"

### 17d — Radar + camera + AI + LiDAR fusion (DESIGN-ONLY, not started —
### blocked on 17c above AND on Phase 14 LiDAR, which is also still fully
### design-only; nothing to fuse against yet on either front)
- [ ] Detections gain velocity for free wherever radar coverage overlaps
      an AI bounding box or a LiDAR cluster (14d) — no tracker required,
      unlike camera-only speed estimation
- [ ] Multi-modal safety rules extend 16 (AI rules engine): proximity ×
      speed × class — "vehicle closing at >X m/s within Y m of a person"
      — same `rule_event` shape 16b already defines, radar is just another
      contributing modality, not a new rule schema
- [ ] Collision/near-miss analytics: haul-truck/AGV closing-speed alarms,
      person-vehicle near-miss detection and statistics (extends 14d)
- [ ] Overlays: velocity vectors and radar-confirmed distance annotated on
      AI boxes, reusing the existing widget/overlay engine (F6)

### 17e — Cluster-mode cross-device, cross-modal fusion (the specific ask:
### radar + AI + other sensors, connected together across the mesh)
- [x] **`cluster::fusion::DetectionFusion` generalized 2026-08-09**
      (`src/cluster/fusion.rs`) — additive, not a rewrite, exactly as
      planned: `record_local`/`correlate_peer` (camera AI fusion) are
      byte-for-byte untouched; `record_local_radar`/`correlate_peer_radar`
      are new methods against their OWN queue, not the shared one — a
      radar zone named e.g. "gate" can never accidentally same-string-
      match an unrelated AI class label. True cross-modal matching (a
      radar zone corroborating a camera AI *label*, not another radar
      zone) is still open — see the note below.
- [x] **No wire protocol change needed, confirmed** — `radar_zone` is
      just a new `Event.kind` value on the same `MessageKind::Event`
      variant every peer already deserializes (`src/main.rs`'s radar
      thread broadcasts it; `cluster_reactions` matches on it exactly
      like `ai_event`).
- [x] **Cross-device corroboration shipped for radar-vs-radar** — two
      mesh nodes' overlapping radar coverage reporting the same zone name
      within `tolerance_ms` now produces a `fused_radar_zone` telemetry
      event, mirroring `fused_detection` for cameras.
- [ ] **True cross-modal matching (radar zone ↔ camera AI label) —
      explicitly NOT attempted**: what counts as "the same object"
      between a class label string and a zone name is a real design
      question (position/geometry correlation, not text matching) that
      deserves its own pass, not a guess bolted onto this one — see
      `fusion.rs`'s header comment.
- [ ] Site-level fused view: `/cluster/status` gaining a per-node sensor
      inventory (camera/radar/LiDAR present + healthy) — not built this
      pass, straightforward follow-up once it's wanted.
- [x] **Decentralization requirement held** — radar fusion runs over the
      exact same peer-to-peer broker-less mesh transport as camera
      fusion, no new central component.

### 17f — Use-case presets (config templates shipped in `config/presets/`)
- [x] **`radar-mock-demo.toml` shipped 2026-08-09** — not a deployment
      scenario, an end-to-end pipeline demo (requested directly: "add
      sample and for now make mock as default"). Radar enabled, `mock`
      backend, three zones exercising all three analytics modes at once.
      **Live-verified, not just unit-tested**: ran the real binary
      against it for 25s and confirmed real log output — "Radar sensing
      active zones=3" at boot, then 148 real `Radar zone event` lines
      (136 presence, 8 line_cross, 4 loiter — loiter correctly firing
      once per continuous visit, not every frame). `[radar]` in
      `config/iiotedge_default.toml` itself is now ALSO enabled by
      default with `type = "mock"` for the same reason, clearly commented
      as a "for now" placeholder to revisit once a real backend exists or
      before any genuine deployment.
- [ ] The four vendor-specific scenarios below are **still NOT built** —
      each names real radar hardware (17a), which doesn't exist yet; a
      preset referencing `[radar].type = "ti_mmwave"` etc. wouldn't
      actually run, same "don't ship a fake production-ready preset" call
      already made for the LiDAR Phase 14e list.
- [ ] `haul-road-safety.toml` (long-range track radar + speed/closing-speed
      alarms, camera verification, mesh-wide corroboration across
      overlapping road-segment nodes)
- [ ] `perimeter-all-weather.toml` (Navtech-class FMCW perimeter radar,
      radar-primary / camera-verification arbitration for dust/fog sites)
- [ ] `gate-people-counting.toml` (short-range mmWave presence + counting,
      camera-optional)
- [ ] `forklift-safety-radar.toml` (radar variant of 14e's
      `forklift-safety.toml` — proximity + direct speed, no AI dependency
      for the core safety function, AI as an enrichment layer on top)

## Phase 18 — Fused 3D spatial world-model & AR overlay (F17) —
## DESIGN-ONLY, NOT STARTED, requested 2026-08-02: "creating a overlays
## like 3d object, person detecting or creating object based on other
## fusion related things... represent it in 3D"

Three genuinely separate layers, only two of which belong in this repo —
same boundary already drawn for QR onboarding (12a) and AI-rule zone-drawing
(16d): **data and pixel-overlay generation are firmware's job; an
interactive 3D viewer/dashboard is a mobile-app/web-dashboard concern**,
built against the data this phase produces, not built here.

Sequencing note: this phase has no real depth data to fuse until Phase 14
(LiDAR) or Phase 17 (radar) actually land — both are still design-only. 18a
is deliberately scoped to work with **camera AI alone** (monocular
ground-plane distance estimate, not true depth) so there's a usable,
shippable result before either sensor exists, and 18b/18c upgrade
automatically — same numeric fields, better accuracy — once real depth
sensors are fused in. Don't build 18b/18c before 14/17 have working HAL
output to consume.

### 18a — Monocular 3D estimate from camera AI alone (buildable today, no
### new sensor required)
- [ ] Camera extrinsics/intrinsics in config: mounting height, tilt angle,
      horizontal/vertical FOV (or focal length + sensor size) — the
      minimum needed for a ground-plane projection, not a full calibration
      pipeline
- [ ] Ground-plane distance estimate: a detection's bounding-box
      bottom-center (same point convention Phase 16 already standardized
      on) + known mounting height/tilt/FOV → approximate real-world
      distance and lateral offset. Accurate for objects touching the
      ground (people, vehicles), not for airborne/elevated objects —
      document that limitation rather than silently returning a wrong
      number
- [ ] New `Object3D { track_id, class, confidence, x, y, z, vx, vy, vz,
      contributing_sensors: Vec<SensorKind> }` type — the one shape 18b/18c
      also produce, so nothing downstream (overlay, telemetry) needs to
      know whether a given field came from monocular estimation or a real
      depth sensor. `x,y,z` in node-relative meters (mounting-pose origin,
      matches Phase 14/17's extrinsic convention); `contributing_sensors`
      starts as `[Camera]` and grows once fusion lands
- [ ] Confidence/uncertainty flagged explicitly on monocular-only objects
      (e.g. a `depth_source: Estimated | Measured` field) — an operator or
      downstream rule must be able to tell "camera's best guess" from
      "radar actually measured this," not silently trust both equally

### 18b — AR-style 3D overlay burned into the 2D video (extends `stream/overlay.rs`)
- [ ] 3D wireframe/box projection: given `Object3D` + camera intrinsics,
      project a 3D bounding box back onto the 2D frame (perspective
      projection, reuses the same extrinsics from 18a/14c/17c) — a
      wireframe cuboid instead of today's flat 2D `DrawBox` rectangle
- [ ] Distance + velocity labels burned into the frame next to each
      object (`"4.2m, 1.3 m/s"`) — reuses the existing text-rendering path
      in `stream/widgets.rs`, not a new text renderer
- [ ] Ground-plane grid / distance rings (optional, config-gated):
      light reference lines burned into the frame so distance is
      visually legible without reading every label, common in AV/robotics
      perception viz
- [ ] `depth_source` (18a) drives rendering style — e.g. dashed wireframe
      for `Estimated`, solid for `Measured` — so the overlay itself
      communicates confidence, not just the underlying data
- [ ] Same TTL/expiry and evidence-index behavior as today's `DrawBox`es
      (`DetectionOverlay`) — this is a richer draw primitive, not a new
      lifecycle model

### 18c — Cross-sensor 3D fusion (blocked on Phase 14/17 HAL landing)
- [ ] Track association: camera detection + LiDAR cluster (14d) + radar
      point/track (17d) referring to the same real-world object get merged
      into one `Object3D` rather than three — nearest-neighbor / gated
      association in the shared node-relative coordinate frame (simplest
      viable approach; full multi-hypothesis tracking only if that proves
      too fragile, same escalation path 16b already documents for loiter
      tracking)
- [ ] `contributing_sensors` and `depth_source` upgrade automatically once
      a radar/LiDAR match lands on an object that started as
      camera-only-estimated — no schema change, just better-populated
      fields on the same `Object3D`
- [ ] Cluster-mode tie-in (17e): a fused `Object3D` is exactly what
      `MessageKind::Event`'s generalized `kind`/modality fusion (17e) is
      meant to carry across mesh peers — one `object3d_event`, not a
      separate wire format per sensor combination

### 18d — Data export (what the mobile-app/dashboard layer consumes —
### **not built from this repo**, same boundary as 12a/16d)
- [ ] GDE telemetry stream of `Object3D` snapshots (position/velocity/
      class/confidence/contributing_sensors), same persist-first pipeline
      everything else already uses — this is the actual "3D representation"
      data contract; a real-time 3D viewer (three.js web dashboard, mobile
      AR view, or similar) is built against this stream by whichever repo
      owns that UI, not here
- [ ] `/cluster/status`-style query for "current `Object3D` set, this
      node" — lets a dashboard poll/subscribe per-node without needing the
      full GDE history pipeline for a live view
- [ ] Explicitly OUT of scope for this phase: any 3D rendering engine,
      web viewer, or mobile AR integration — flagged the same way 16d
      flags mobile UI work, not authorized here

## Phase 19 — Smart-home ecosystem integration (Home Assistant / Zigbee /
## Z-Wave / Matter) — 19a/19b DONE 2026-08-04, 19c/19d/19e (Matter) DONE
## 2026-09-06, requested 2026-08-04: "any application relate to iot home
## automation using these features, by connecting multiple devices...
## Zigbee/Z-Wave/Matter/Home Assistant integration"

Industry research done before writing this entry (Home Assistant's MQTT
Discovery spec, Zigbee2MQTT/Z-Wave JS UI's MQTT interfaces, the Matter 1.5
Camera specification and the current Rust Matter SDK landscape) — same
"research before design" discipline as Phase 14/16/17.

This firmware is not becoming a home-automation hub — its southbound
drivers stay industrial (Modbus/CAN/serial/OPC UA) and its identity stays
"config-driven edge vision platform." What this phase adds is **two-way
bridges** so it plugs into an existing smart-home ecosystem the same way
it already plugs into a PLC/SCADA one: consumer-protocol sensor events
(door/motion/leak) feed the *existing* correlation engine exactly like
industrial ones do today, and this camera's own events/streams surface as
native entities in the ecosystem someone's already running, instead of
requiring a bespoke app.

Sources consulted: [Home Assistant MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/),
[HA MQTT binary_sensor](https://www.home-assistant.io/integrations/binary_sensor.mqtt/),
[Zigbee2MQTT MQTT topics/messages](https://www.zigbee2mqtt.io/guide/usage/mqtt_topics_and_messages.html),
[Z-Wave JS UI](https://github.com/zwave-js/zwave-js-ui),
[CSA: Matter 1.5 introduces Cameras](https://csa-iot.org/newsroom/matter-1-5-introduces-cameras-closures-and-enhanced-energy-management-capabilities/),
[Matter 1.5 camera WebRTC explainer](https://www.matteralpha.com/explainer/what-is-a-matter-camera-and-how-does-it-work),
[rs-matter (project-chip)](https://github.com/project-chip/rs-matter).

### 19a — Home Assistant integration (buildable now, highest confidence)
- [x] **Two things already work today with zero firmware changes**
      (2026-08-04) — documented rather than built: HA's built-in Generic
      Camera / ONVIF integrations already consume this firmware's
      existing RTSP + ONVIF services directly; an `[[ai.rules]]`
      `webhook` action can already POST straight to an HA automation's
      webhook trigger URL. Both proven by this firmware's existing
      feature set, not new capability.
- [x] **MQTT Discovery publisher** (2026-08-04, `src/homeassistant.rs`):
      `<discovery_prefix>/<component>/<device_id>/<object_id>/config`
      retained messages per the HA spec. Riding the SAME broker/identity
      as the command channel (`edge.toml`'s `[northbound.mqtt]`), not a
      second connection — a deliberate coupling, not the "no second
      broker connection" phrasing originally planned here, because it's
      also what makes the next line work with zero extra code. Publishes
      a `binary_sensor` per configured `[[ai.rules]]` entry (momentary,
      `off_delay_s`), one aggregate tamper `binary_sensor`, one motion
      `binary_sensor` per zone (or one `"frame"` zone in whole-frame
      mode) — PTZ preset buttons **not built** (no fixed preset
      enumeration to discover at boot; flagged, not attempted).
- [x] **HA buttons reuse the existing command channel directly**
      (2026-08-04) — turned out simpler than planned: a `button`
      entity's `command_topic` can just BE
      `iiotedge/<group>/<node>/cmd` with `payload_press` set to the
      exact `{"cmd":"snapshot","token":"..."}` JSON
      `commands::handle_command` (`src/commands.rs`) already parses, so
      pressing Snapshot/Clip in HA needed **no new command-ingestion
      code at all** — the originally-planned separate wiring step was
      unnecessary once this was understood.
- [x] Config: `[home_assistant]` — `enabled`, `discovery_prefix` (default
      `"homeassistant"`), `device_name` (empty = `system.device_id`),
      `off_delay_s`. 7 unit tests (topic shape, entity JSON, button
      payload matches the real command schema).

### 19b — Zigbee / Z-Wave southbound bridge (buildable now, via existing
### bridge software — not a native radio stack) — DONE 2026-08-04
- [x] **Deliberately not a Zigbee/Z-Wave radio stack** — same "don't
      build infrastructure blind" call as 12c/12d: no mature Rust
      Zigbee/Z-Wave MAC/PHY crate at production quality, no radio
      hardware on the reference device, and Zigbee2MQTT / Z-Wave JS UI
      are already the de facto standard open-source bridges most
      Home-Assistant-adjacent users already run — bridging to those is
      far less risk than reimplementing a radio protocol stack.
- [x] New southbound "shape" alongside the existing serial/CAN/Modbus
      ones (`src/mqtt_bridge.rs`): an MQTT subscriber to a configurable
      `topic_filter` (`"zigbee2mqtt/#"` / `"zwave/#"` in practice) —
      generalized to any JSON-over-MQTT source, not hardcoded to one
      bridge product, since the mechanism is identical either way.
- [x] Feeds the SAME correlation engine (`correlation.rs`) already wired
      to industrial southbound tags, via a **direct `Processor::process`
      call**, not a registered iiotedge-lib southbound driver — this
      bridge isn't part of edge.toml/iiotedge-protocols, so there's no
      engine ingest path to ride; calling the tap directly (the same
      `Arc<CorrelationProcessor>` the engine's own tap wraps) reuses 100%
      of the existing rule-matching/snapshot/clip logic with zero
      duplication. `source_id` is the raw MQTT topic
      (e.g. `"zigbee2mqtt/front_door"`), so `[[correlation.rules]]`
      `source_prefix` matches it exactly like `"serial/scanner1"` today.
- [x] Config: `[[mqtt_bridge]]` — `name`, `host`, `port`, `topic_filter`,
      `username`, `password`. Boot-time validation (non-empty
      name/host/topic_filter) plus a boot warning (not a hard failure) if
      `mqtt_bridge` is configured but `[correlation]` has no rules to
      match against. 2 unit tests using real Zigbee2MQTT JSON shape
      (`{"contact":false,"battery":87}`).

### 19c — Matter support — SHIPPED 2026-09-06 (src/matter/)
Both blockers below were re-verified (not assumed) before writing any
code, and the second one turned out to be WRONG on first pass — corrected
before implementation started, not after:
- **Blocker 1 (spec maturity)** still stands as background context but
  didn't block anything: Matter 1.5's Camera device type (0x0142) plus
  WebRTC Transport Provider (0x0553)/Requestor, Camera AV Stream
  Management, and Zone Management clusters are real and, per rs-matter's
  own reference example comments, already interoperate with at least one
  major controller (SmartThings) as of mid-2026.
- **Blocker 2 (Rust SDK support) was investigated wrong the first time.**
  An initial pass over a stale `rs-matter` capability-matrix doc
  concluded no camera clusters existed, which would have meant scoping
  this down to non-video clusters only. Fetching and reading the crate's
  actual reference example (`examples/src/bin/webrtc_camera.rs`, ~1400
  lines, real working code) directly showed that conclusion was wrong —
  camera support is real and implemented. This was reported back and
  re-confirmed with a wider scope before writing `src/matter/`, rather
  than silently proceeding on the stale finding.

**What's real, in `src/matter/`:**
- Commissioning: PASE, QR/manual pairing code (printed at boot while
  uncommissioned), built-in cross-platform mDNS discovery
  (`src/matter/mdns.rs`, `if-addrs` + a raw multicast socket — no
  platform-specific D-Bus/zeroconf backend needed).
- WebRTC Transport Provider (`src/matter/camera.rs`): real SDP offer/
  answer + trickle ICE via a real `str0m::Rtc` per session, adapted
  closely from rs-matter's own proven reference implementation.
- Live H.264 media: a genuine tap of this firmware's own capture frames
  (`src/matter/encoder.rs`, a second independent GStreamer encode
  pipeline fed by a third `FrameRouter` consumer queue — NOT the
  reference's preloaded static file replayed on a timer). Real trade-off:
  when both RTSP and an active Matter viewer are running, frames get
  encoded twice (no tap point into `RtspStreamer`'s internal pipeline
  exists today) — see encoder.rs's header.
- Camera AV Stream Management: config sourced from this device's real
  `[camera]`/`[stream]` settings, not hardcoded resolution/fps.
- Zone Management: pre-seeds this device's real `[[ai.rules]]` zones
  (3+ point polygons only — `line_cross` rules have no Matter Zone
  Management equivalent) as manufacturer/read-only zones via
  `add_mfg_zone`; a controller can see them but not create/modify/remove
  them, since they're config-owned by this firmware, not a second source
  of truth.
- Off by default (`[matter].enabled = false`) — unlike radar's mock,
  enabling this opens a real UDP+TCP listener and an open pairing window
  on the LAN, so it's an explicit opt-in, same posture as onboarding's QR.

**Deliberately NOT implemented, and why:**
- [ ] Real device attestation. No CSA-issued certificate chain exists for
      this firmware — commissioning uses rs-matter's own `TEST_DEV_ATT`/
      `TEST_DEV_COMM`/`TEST_DEV_DET` (the same constants `chip-tool`, the
      reference Matter controller CLI, expects out of the box). Same
      "for now" call already made for Phase 12c's onboarding QR.
- [ ] Camera AV Settings (mechanical/digital PTZ over Matter). Matter's
      MPTZ model is absolute-position based (go-to-angle in hundredths of
      a degree); this firmware's only PTZ backend (Pelco-D) is
      continuous-move + preset based with no position feedback — there's
      no honest way to answer "what angle are you at." Even rs-matter's
      own reference example only claims DIGITAL PTZ, not mechanical.
      Revisit if a PTZ head with real position feedback ever shows up, or
      once the `MECHANICAL_PRESETS` feature bit (independent of
      mechanical pan/tilt/zoom, maps more plausibly onto
      `PtzController::set_preset`/`goto_preset`) is researched.
- [ ] Zone triggers are logged only — not yet wired to actually arm/disarm
      `ai::rules::RuleEngine` zones live.
- [ ] Shared encode: Matter's live H.264 tap runs its own encoder rather
      than sharing RTSP's — see encoder.rs's header for the real CPU-cost
      trade-off and what tapping `RtspStreamer` directly would take.

**A real, generalizable trap hit and documented, not just for this
phase:** the upstream `rs-matter` GitHub repo's `examples/` directory
(fetched from its `main` branch for reference) is AHEAD of the actual
published `rs-matter = "0.3.0"` crate this project depends on — in at
least two ways found so far: it pins `rand = "0.10"` where the installed
0.3.0 crate's own `Cargo.toml` resolves `rand_core = "0.6"`, and its
`.chain()` calls pass bare closures as matchers where the installed
0.3.0 has no blanket `Matcher` impl for closures at all (only for
`EpClMatcher` and `&M`). Both were caught by checking the actually
*installed* crate source under `~/.cargo/registry/` before trusting the
newer doc — the same "verify against the real, pinned dependency, not an
assumption or a newer upstream example" discipline the WHIP
`whipclientsink` → `whipsink` bug taught earlier in this project.

**Real bugs `cargo check`/`clippy` could never have caught, found only by
actually running the compiled binary** (same "live-verify, don't trust a
green typecheck" discipline as the radar smoke test — `cargo check`
skips codegen/linking entirely, so a link-time or runtime-only failure is
invisible to it):
- A genuine link-time gap: `async-executor`'s `LocalExecutor` (the
  per-session WebRTC driver, `Str0mShared::drive`) pulls in
  `embassy-executor-timer-queue` transitively, which references an
  extern symbol only DEFINED once something provides a concrete
  timer-queue backend. `cargo build --bin` failed to link
  (`___embassy_time_queue_item_from_waker` undefined) even though
  `cargo test` — different feature unification — happened to link fine
  for unrelated reasons. Fixed by adding `embassy-time-queue-utils`
  (`generic-queue-64` feature) explicitly, the same pin the reference
  example's own Cargo.toml carries for the identical symbol.
- mDNS port 5353 was already held by another process (Chrome's own mDNS,
  observed live on the dev host) — `SO_REUSEADDR` alone doesn't let two
  listeners share a UDP port on macOS/BSD; needed `SO_REUSEPORT` too
  (`src/matter/mdns.rs`).
- Enabling rs-matter's `"groups"` Cargo feature (for the `GroupsHandler`
  cluster) also activates its Groupcast multicast-messaging transport
  path, which tries to join an IPv6 multicast group on interface index 0
  ("any") — confirmed to fail with `StdIoError` on this dev host's
  macOS/BSD IPv6 stack, unlike Linux which is more lenient. Removed the
  Groups cluster and the feature entirely: not needed for a Camera
  device (it's a lighting/scene-control concept), and real camera
  functionality now beats chasing a platform multicast quirk for a
  cluster this device type doesn't need.
- `join_multicast_v4` (`IP_ADD_MEMBERSHIP`) on an IPv6-domain socket is a
  real, confirmed cross-platform inconsistency: fails with `EINVAL` on
  macOS/BSD even with `IPV6_V6ONLY` off, expected to work on the Linux
  target. Rather than aborting the whole Matter node over IPv4-mDNS
  specifically, `mdns.rs` now logs a warning and continues IPv6-only —
  the same socket's IPv6 join (which succeeds) already covers this
  firmware's own send/receive needs.
- After all of the above: a full live boot with `[matter].enabled = true`
  produces a real `SetupQRCode: [MT:...]` payload, binds UDP+TCP
  transport, starts the live H.264 encode pipeline, and stays up
  (verified over multiple runs, RUST_LOG=info and =debug) — no
  regressions to the other 167 unit tests (one, `cluster::wifi`'s
  loopback-datagram test, is a pre-existing, self-documented flake under
  heavy build-concurrency CPU load — confirmed by re-running it alone
  once load settled).

**Onboarding QR for Matter, added alongside the fix pass** (requested:
"before this please make a onboard endpoint enable for matter as well so
i can just scan qr code to add"): `GET /onboarding/matter-qr.png`
(`src/core/metrics.rs`, gated by `[security].command_token` exactly like
the existing `/onboarding/qr.png`) renders the real Matter `MT:...` setup
code as a scannable PNG — `src/matter/mod.rs::setup_qr_text` computes the
same payload `Matter::print_standard_qr_text` logs at boot, via the
public `rs_matter::pairing::qr::QrPayload::new_from_basic_info`
constructor, independent of the live `Matter` instance (pure function of
`device_id` + the fixed test commissioning data), computed once at boot
and reused for every request. Verified live: `curl` with a valid token
returns a real 222x222 scannable PNG (HTTP 200); without one, a proper
401. Same credential posture as the pairing code itself — whoever holds
it can commission the device into their own fabric — so it's gated, not
public, despite Matter's commissioning window also being open at that
point.

**Deployed to real hardware and commissioned into a real Apple Home
fabric, 2026-09-06** — cross-compiled via `make docker-release` (first
time this dependency stack has been built for aarch64; `make docker-check`
verified clean first), deployed to the production Radxa Zero 3E
(`cam_line1_inspect_04`), `[matter].enabled = true`, then scanned via
`GET /onboarding/matter-qr.png` straight into the iOS Home app. Real,
concrete result, resolving what Blocker research could only leave
unconfirmed for Apple specifically:
- **Commissioning succeeds fully.** Apple Home accepts the device (with
  the expected "not certified to work with HomeKit" notice, since
  attestation is test-only — no real CSA cert) and correctly reads real
  Basic Information cluster attributes: Manufacturer "IIoTEdge", Serial
  Number = this device's real `device_id`, Model = "fusion-firmware
  Camera", Firmware = "1". The entire PASE/attestation/basic-info
  pipeline is confirmed interoperable with a real, major, unmodified
  commercial Matter controller — not just internally self-consistent.
- **No live view or camera controls appear in Apple Home.** This is
  Apple's controller-side gap, not a firmware defect: the Home app has
  no rendering path for Matter's Camera device type / WebRTC Transport
  Provider cluster yet, so it shows the accessory as a bare generic
  device with only the attributes it already knows how to display. This
  is the SAME uncertainty flagged when this phase started ("I did not
  find confirmation that Apple Home has shipped support for the
  camera-specific clusters") — now empirically confirmed rather than
  merely unconfirmed. SmartThings remains the only controller with real
  evidence (rs-matter's own reference-example comments) of rendering
  Matter camera clusters; worth testing against if access to it exists.
- Real hardware also surfaced one thing the dev-host testing couldn't:
  restarting the service via systemd took ~90s longer than expected —
  SIGTERM timed out and systemd had to SIGKILL the old process. This
  happened to the PREVIOUS (Matter-disabled) process during the restart
  that flipped `[matter].enabled` on, so it's very likely a pre-existing
  graceful-shutdown characteristic of this firmware on real Rockchip
  hardware (GStreamer/RKNN session teardown?), unrelated to Matter —
  flagged here as a real, separate thing worth investigating, not yet
  root-caused.

### 19d — Generic, multi-device-type Matter support — SHIPPED 2026-09-06
### (src/matter/onoff.rs), requested by Santosh: "now lets support all the
### other features supported by matter 1.5 or 1.6 not just camera like all
### the controls ... for example if i deploy this firmware on light bulb
### then it should be able to treat as that ... this is generic firmware
### right please do add and make it production ready"

**Scope call, made explicit rather than silently narrowed:** literally
every Matter 1.5/1.6 device type (door locks, thermostats, HVAC, energy
management, closures, media players, …) is not something this pass
builds, and won't be unless real hardware exists behind it — fabricating
clusters with no real backing is exactly the kind of unverified-hardware
claim this project has consistently avoided (mock-vs-real camera/radar
backends, no mechanical PTZ over Matter, no LiDAR). What this phase
builds instead: a genuinely generic ARCHITECTURE (config selects which
Matter endpoints exist, not a compile-time fixed personality) plus the
one additional device type this firmware can honestly back with real
hardware today — On/Off Light/Switch (0x0100), backed by a real GPIO
output line via `gpio-cdev` (same crate/convention `[[ai.rules]]`'s
`gpio_output` action already uses, held persistently open here instead
of pulsed, since Matter's OnOff is a durable state not a momentary
trigger). This is literally what makes the light-bulb example real:
`[matter.camera].enabled = false` + `[matter.onoff].enabled = true` with
a real `gpio_chip`/`gpio_line` deploys this exact firmware as a plain
Matter light switch, no camera clusters at all.

Checked what's actually available before building: `rs-matter 0.3.0` has
NO `OccupancySensing` or `BooleanState` cluster implementation anywhere
in the crate (verified by grepping the installed source directly — a
real, hard constraint, not a choice) — so mapping AI-rule presence zones
or tamper detection onto Matter sensor clusters isn't buildable against
this dependency version. `OnOff`, `LevelControl`, and `ColorControl` DO
exist and are real; only `OnOff` is used, since this firmware has no
PWM/dimmer or RGB driver behind it to honestly back the other two.

**The real Rust architectural constraint this had to work around:**
rs-matter's cluster-chaining (`.chain()`) is an INHERENT method whose
return type changes on every call — genuinely different concrete types
per combination, not a design choice — so which clusters get wired into
the Interaction Model must be fixed at compile time; there's no
`Vec<Box<dyn Handler>>`-style dynamic chain in this crate. Solved by
ALWAYS constructing and ALWAYS chaining every possible device type's
handlers (cheap in itself — see below for the one real exception) and
making only the Matter `Node`'s endpoint LIST config-driven: an endpoint
absent from that list is never routed to by the Interaction Model, so a
disabled device type's always-present handler is simply unreachable, not
actually live. `camera::ChainExt` (the blanket `.chain()`-providing trait
this required in the first place, see camera.rs) is reused as-is by
onoff.rs; adding a THIRD device type means writing its own
`<type>_endpoint()` + `chain_handlers()` following the identical
shape — `matter::mod::run` is the only file that needs to learn about a
new device type at all.

**One exception to "always construct, never mind resources":** the live
H.264 encode pipeline (a real GStreamer pipeline + thread) only starts
when `[matter.camera].enabled` — running it for an onoff-only deployment
would be genuinely wasteful and, on hardware with no camera at all,
likely to fail loudly for no reason.

**Two real bugs found only by live-running both endpoint combinations**
(camera+onoff together, and the onoff-only "light bulb" case — `cargo
check`/`clippy` cannot catch either, matching this whole feature's
established "verify by actually running it" discipline):
- `RelayOnOffHooks::new` originally opened the real GPIO line whenever
  `gpio_chip` was non-empty, without checking `cfg.enabled` first — a
  disabled switch with a stale `gpio_chip` left over from a copied config
  would have opened real hardware it had no business touching. Fixed:
  the `enabled` check moved inside the constructor itself (it's called
  unconditionally per the always-construct design above), not just at
  the call site.
- Enabling `[matter.onoff]` while `[matter.camera]` was OFF caused an
  infinite "Matter encode queue full! Dropping frames" warning storm —
  `main.rs`'s third `FrameRouter` consumer queue was wired up whenever
  `[matter].enabled`, with nothing to ever drain it when the camera
  endpoint specifically was disabled. Fixed by gating that queue on
  `[matter].enabled && [matter.camera].enabled` together. Fixing THIS
  then caused a second, more serious regression caught by immediately
  re-testing rather than assuming success: `main.rs` was using
  "does the camera frame tap exist" as a proxy for "should the Matter
  node spawn at all" — coupling that had been harmless while camera was
  the only endpoint, but with the tap now conditional on
  `[matter.camera]` specifically, an onoff-only config silently never
  spawned the Matter node AT ALL (`matter::spawn` requires no arguments
  changed, verified via a targeted `eprintln!` diagnostic that the config
  parsed correctly and the binary contained the new code — the bug was
  purely in main.rs's control flow). Fixed by spawning Matter directly on
  `app_config.matter.enabled` and threading the frame tap through as a
  genuine `Option<Receiver<FrameHandle>>` instead of using its presence
  as a stand-in for a different condition.

Live-verified after both fixes: `camera=true,onoff=true` boots with
`endpoints=3`; `camera=false,onoff=true` (the light-bulb case) boots with
`endpoints=2`, zero queue-full warnings, real `SetupQRCode` payload,
stays up. `cargo check`/`clippy -D warnings`/`test` all clean (167/167
tests passing) after every change in this phase.

### 19e — Full color light (hue/saturation, XY, color temperature,
### dimming) — SHIPPED 2026-09-06 (src/matter/light.rs), requested by
### Santosh: "now lets support all the features avaible in matter 1.6
### please list out all ... like in llight hue support etc not just lght
### on off support all the features" / "not just light bulb kindly get all
### the matter feature ... use latest protocl 1.5 or 1.6"

**What was actually checked before answering "list all of Matter 1.6"**:
grepped the installed `rs-matter 0.3.0` source directly (not the spec, not
memory) for every cluster it implements. The real ceiling for this
firmware isn't the Matter spec version — it's this one dependency. Its
entire device-application cluster surface is: `on_off`, `level_control`,
`color_control` (Lighting) and the Phase 19c camera cluster set
(`webrtc_prov/req`, `cam_av_stream`, `cam_av_settings`, `push_av_stream`,
`chime`, `zone_mgmt`) — plus infra/commissioning clusters already in use
(ACL, NOC, groups, scenes, diagnostics, OTA, power source, identify,
binding, ICD management, time sync). Door Lock, Thermostat, Fan, Window
Covering, every environmental-sensor cluster, every appliance cluster
(refrigerator/laundry/dishwasher/oven/RVC), energy management (EVSE/water
heater/solar/battery), and Closures have **no implementation anywhere in
this crate** — not "not wired up," genuinely absent source code. Even
`devices.rs`'s own `DEV_TYPE_GENERIC_SWITCH` constant is metadata-only:
the crate lists the device-type ID but never implements a `Switch`
cluster to back it. Building any of these would mean writing a whole
cluster from raw TLV/Interaction-Model primitives from scratch — a
multi-week undertaking per cluster, not "wire up a hook" — and most have
no real actuator on this board (a camera + a few GPIO lines) to honestly
back them anyway. This was reported to Santosh plainly rather than either
refusing to build anything more, or silently faking clusters the crate
doesn't have.

What **is** real, present, and matches the explicit "light hue" ask:
`LevelControl` and `ColorControl`, both designed by rs-matter to couple
with `OnOff` on the same endpoint (their handlers take the coupled
handler as a type parameter) — exactly how a real Matter bulb composes.
Santosh chose: in-memory-only state for now (no PWM/RGB driver exists on
this board — same honest-fallback shape `onoff::RelayOnOffHooks` already
uses when no GPIO is configured), and Hue&Saturation + XY as the color
model. Shipped broader than the minimum ask: the full color feature set
(HS + Enhanced Hue + Color Loop + XY + Color Temperature), mirroring
rs-matter's OWN internal test fixtures
(`on_off`/`level_control`/`color_control`'s `test::Test*DeviceLogic`)
near-verbatim for cluster metadata — the crate's own known-good
configuration, lowest risk to get right rather than hand-trimming feature
bits/attrs/cmds to a smaller, unverified combination.

**Architecture**: a third, independent endpoint (id 3, `DEV_TYPE_
EXTENDED_COLOR_LIGHT` / 0x010D) — deliberately NOT a "mode" of
`[matter.onoff]`'s relay, since Matter's cluster-chaining being fixed at
compile time (see 19d above) means two different cluster sets can't
safely share one endpoint id if both were ever chained onto it at once
(they'd both claim the same endpoint+cluster match). Keeping it a fully
separate endpoint sidesteps that: `[matter.light]` can be enabled
independently of, or alongside, `[matter.camera]`/`[matter.onoff]`, with
zero collision risk, following the exact same "always construct, only the
endpoint LIST is config-driven" pattern as before. `OnOffHandler`,
`LevelControlHandler`, and `ColorControlHandler` are cross-coupled via
their real `.init()` calls (`onoff.init(Some(level))`,
`level.init(Some(onoff))`, `color.init(Some(onoff))`) once all three are
`'static`, so `MoveToLevelWithOnOff` and friends behave like a real
Matter Lighting device, not three independently-acting clusters.

Live-verified: `camera=false,onoff=true,light=true` boots with
`endpoints=3`, and `camera=true,onoff=true,light=true` boots with
`endpoints=4` — both with a valid `SetupQRCode`/pairing code, zero
panics (a cluster metadata mismatch against rs-matter's own conformance
`validate()` checks would have crashed the process immediately at
startup — it didn't), zero queue-full warnings. `cargo check`/
`clippy -D warnings`/`test` all clean (167/167 tests passing).

## Phase 19f — Thermostat (Matter 1.0 HVAC) — SHIPPED 2026-10-01
## (src/matter/thermostat.rs), requested by Santosh: "now lets support all
## the features avaible in matter 1.6 ... start implemetning all one by
## one" -> "work on HVAC and make it conigurale same as we have then
## appliances then energy, then media casting before tat complete sensors
## and switchs and rest of them add allfeatures"

A real architectural wall, hit and worked through: rs-matter 0.3.0's
entire device-application cluster surface is Lighting (on_off/
level_control/color_control, 19d/19e) plus the Phase 19c camera clusters —
confirmed by reading the installed crate's source directly. Thermostat,
Fan, Door Lock, Window Covering, every environmental-sensor cluster, every
appliance/energy/media cluster: NONE of these have any implementation in
this crate, regardless of Matter spec version. Building any of them is a
fundamentally bigger job than everything shipped so far (which was "wire a
hook into an existing rs-matter cluster implementation") — it means
hand-implementing rs-matter's own low-level, public, non-sealed `Handler`
trait directly: hand-declaring the `Cluster`/`Attribute`/`Command`
metadata (no `FULL_CLUSTER` decl constant to start from and filter, unlike
every cluster before this one) and hand-decoding/encoding TLV attribute
reads/writes and command invokes.

- [x] **Thermostat (cluster 0x0201)**, the first cluster built this way —
      verified every mechanism against the installed rs-matter source
      before writing a line (not guessed): `Handler::read/write/invoke/
      bump_dataver`, `AttrDetails`/`CmdDetails`, `ReadReply::with_dataver`
      -> `Reply::set`, `InvokeReply`, multi-field command decoding
      (`TLVElement::structure()?.ctx(n)?` + `FromTLV::from_tlv`, mirroring
      the one real hand-decoded command payload anywhere in rs-matter's
      own source, `time_sync`'s `SetTimeZone`), and confirmed global
      attributes (ClusterRevision/FeatureMap/...) are resolved generically
      by the framework from `Cluster` metadata and never reach a cluster's
      own `read()`.
- [x] **Attributes**: `LocalTemperature` (read-only — the one REAL signal,
      backed by `health::read_soc_temp_c()`, the SoC's own thermal-zone
      reading already used for the Prometheus gauge; reports `null`
      honestly on a failed read, never a fabricated 0°C — this is DEVICE
      temperature, not room-ambient, stated as such in the module header),
      `OccupiedCoolingSetpoint`/`OccupiedHeatingSetpoint` (read-write,
      in-memory, default 24.00C/20.00C), `ControlSequenceOfOperation`
      (read-only, fixed "CoolingAndHeating" — the least-committal honest
      value given there's no real equipment), `SystemMode` (read-write,
      in-memory, validated against Off/Auto/Cool/Heat — an out-of-range
      write is rejected with `ConstraintError`, not silently accepted).
      **Command**: `SetpointRaiseLower` (adjusts the named setpoint(s) by
      the given 0.1C-step amount).
- [x] **Confidence note, carried into the module header verbatim**: this
      cluster has no crate-provided `validate()` safety net the way
      on_off/level_control/color_control do (a cluster-metadata mistake
      there panics loudly at startup; a mistake here would only surface
      against a real controller's read/write). The cluster/attribute/
      command ID numbers are the Thermostat cluster's long-stable base-
      spec values, consistent across every public Matter SDK/sample this
      project has encountered — NOT independently checked against the
      official CSA spec PDF (no copy exists in this project). Mechanically
      verified and live-tested against this crate's own IM dispatch; does
      NOT yet carry on_off/level/color's multi-controller confidence until
      it has had a real controller (Apple Home / chip-tool) read+write
      pass — `chip-tool` isn't available in this dev environment (no
      Homebrew formula, and building connectedhomeip from source was
      judged disproportionate for this pass), so that remains the next
      real-hardware verification step, same as every other Matter feature
      here eventually got.
- [x] **A real bug found only by a full `cargo build`** (not `cargo
      check`/`clippy`/`test`, none of which perform this step): adding a
      4th always-chained device type (thermostat, on top of camera/onoff/
      light) pushed the `ChainedHandler<M, H, T>` generic nesting deep
      enough that computing the async state-machine layout for
      `InteractionModel::run()` overflowed rustc's default query
      recursion limit (128) — "queries overflow the depth limit". Fixed
      with `#![recursion_limit = "256"]` on the crate root — the standard,
      accepted fix for genuinely deep (not buggy) generic nesting, not a
      workaround for a real bug. Flagged in main.rs's own comment as
      something to raise further as more device types join the same
      always-chained pattern (appliances/energy/media/sensors/switches
      next, per Santosh's ordering above) — this is now a real, recurring
      cost of that architecture's scaling, not a one-off.
- [x] `[matter.thermostat]` config (endpoint id 4, no hardware fields — no
      HVAC equipment exists on this board to configure a relay/contactor
      for). Live-verified: `camera=true,thermostat=true` (onoff/light
      off) boots with `endpoints=3`, valid `SetupQRCode`/pairing code,
      zero panics, rest of the stack (RTSP/radar/mDNS/commissioning)
      unaffected. `cargo check`/`clippy -D warnings`/`test` all clean
      (167/167 tests passing).
- [ ] **Not yet done, same Santosh-specified order**: Fan (0x0202, HVAC's
      other half — could be REAL if a relay is wired, reusing onoff.rs's
      exact GPIO pattern, unlike Thermostat), then Appliances, then
      Energy management, then Media/casting, then completing Sensors
      (motion/tamper -> a hand-rolled BooleanState-shaped cluster — the
      one sensor candidate backed by signal this firmware already
      computes for real) and Switches (the SD-export-button pattern ->
      Generic Switch, if repurposed as a general-purpose input), then
      "the rest" of the Matter Device Type Library. Each is its own
      from-scratch `Handler` implementation at this same cost/risk level —
      paused here to check in and verify this first one solidly rather
      than compounding risk across several unverified hand-rolled
      clusters at once.

## Phase 19g — GENERIC multi-device Matter platform (plan, written
## 2026-10-01), requested by Santosh: "as you know this firmware is
## generic so can you please plan accordingly?? and also go through the
## latest matter protocol feature and enable that as well"

### 19g.0 — CORRECTION to what 19c-19f said about rs-matter (verified 2026-10-01)

Phases 19e/19f (and the README/FEATURES text, now corrected) claimed that
rs-matter had "no implementation at all" for anything beyond Lighting +
Camera. That was WRONG — the conclusion came from reading only
`dm/clusters/app/`, the directory of ready-made *application handlers*
(hooks + spec-rule enforcement). rs-matter ships three layers, and I only
looked at one:

- **Pattern A — typed, spec-generated declarations** (`dm::clusters::decl::*`,
  generated by its build.rs from the Matter IDL): per-cluster `ClusterHandler`
  / `ClusterAsyncHandler` traits with one typed getter/setter per attribute
  and one `handle_*` per command, `FULL_CLUSTER` metadata, typed builders.
  Present in 0.3.0 for ~150 clusters — thermostat, fan_control, door_lock,
  window_covering, boolean_state, occupancy_sensing, every *_measurement,
  air_quality + all concentration clusters, smoke_co_alarm, switch,
  electrical_*_measurement, energy_evse, water_heater_*, closure_control,
  closure_dimension (1.5), soil_measurement (1.5), ambient_context_sensing,
  av_analysis, proximity_ranging, commodity_tariff, media (media_playback,
  channel, content_launcher, ...), appliance mode clusters, rvc_*, and more.
- **Pattern B/B1 — application handlers with hooks** (`dm::clusters::app::*`):
  in 0.3.0 only on_off, level_control, color_control + camera clusters. In
  0.4.1 (crates.io, 2026-09-17) additionally generic Mode/ModeSelect handlers.
  On git `main` only (merged Sep 22-26, NOT in any release yet): Thermostat
  (`ThermostatHooks`, HEAT/COOL/AUTO), FanControl (`FanControlHooks`),
  ElectricalPowerMeasurement, ElectricalEnergyMeasurement, PowerTopology.
- **Events** are supported by the framework (`HandlerContext::emit_event`,
  available in each handler's background `run()`), contrary to a stale line
  in upstream's docs.

Consequence: Thermostat (19f) should NOT have been raw-`Handler` TLV. The IDs
I hand-wrote match the generated data (verified: attrs 0/17/18/27/28, cmd 0),
so it is correct, but it gets re-done on the typed layer in 19g.3. Every other
device type is "implement a small typed trait", not "write a protocol stack".

### 19g.1 — Matter 1.5 / 1.6 — what is actually new, and what it means HERE

Matter 1.6 shipped 2026-06-17 (1.6.1 specs since). It adds NO new device
categories. Matter 1.5 (2025-11-20) added cameras, closures, soil sensors and
energy-tariff clusters. Honest per-feature status for this firmware:

| Feature | Status |
|---|---|
| Camera (1.5): WebRTC, AV streams, zones, chime | DONE (19c) |
| Closures (1.5): ClosureControl / ClosureDimension | PLANNED 19g.4 — real: relay(s) + limit-switch inputs |
| Soil measurement (1.5) | PLANNED 19g.2 — via any numeric signal (typed `soil_measurement` cluster exists in rs-matter 0.4.1) |
| AmbientContextSensing + AVAnalysis (1.5/1.6 camera-AI) | PLANNED 19g.2c — typed `ambient_context_sensing` / `av_analysis` / `ambient_sensing_union` exist in rs-matter 0.4.1; maps this firmware's REAL AI detections (person/vehicle/object, rule events) onto Matter. First step, below: `ai:` sources + hold time so a person/vehicle detection becomes an Occupancy (VISION) sensor |
| Energy: ElectricalPower/EnergyMeasurement, PowerTopology | PLANNED 19g.5 — real if a meter feeds a signal |
| Energy: tariffs / grid conditions / EVSE / water heater | VIRTUAL-ONLY unless a real integration exists (opt-in, labelled) |
| 1.6 Thermostat Suggestions | PLANNED 19g.3 (minimal) — typed layer already has the attrs/commands |
| 1.6 Security-sensor event history | PARTLY: event emission is now proven (Generic Switch); StateChange / OccupancyChanged on the existing sensors still to add (19g.2) |
| 1.6 Joint Fabric | N/A — ecosystem/controller side; a plain device node just joins the fabric |
| 1.6 NFC commissioning | N/A on this hardware (needs an NFC tag/reader); QR + manual code stay |
| 1.6 Partitioned CRLs, 1.4.2 CRLs | N/A — attestation/DCL infrastructure, not device logic |
| Multi-admin (several ecosystems at once) | ALREADY in rs-matter; add an "open commissioning window" command + docs (19g.7) |
| Real device attestation (DAC/PAI/CD) | needs CSA certification; make vendor/product IDs + cert paths CONFIGURABLE for integrators (19g.7) |

### 19g.2+ — the plan

**Architecture problem to solve first** (why the current shape cannot scale):
(a) every device type is a singleton `[matter.<type>]` with a fixed endpoint
id — a board with 4 relays or 6 sensors is impossible; (b) the compile-time
`ChainedHandler` nesting grows with every cluster (already forced
`#![recursion_limit = "256"]` at 4 device types — it will keep growing);
(c) each new type edits mod.rs/config.rs in ~6 places.

**Design (generic, config-driven, one binary):**
1. `[[matter.endpoints]]` array of tables — `kind`, optional `name`, optional
   pinned `endpoint` id, plus a `source` (what a sensor reads) and/or `sink`
   (what an actuator drives). The existing `[matter.camera|onoff|light|
   thermostat]` sections keep working unchanged (translated internally to
   registry entries with their historical endpoint ids 1-4, so already-paired
   controllers are unaffected).
2. **Enum-dispatch router instead of nested chains**: one `Router` implementing
   `AsyncHandler`, holding `Vec<(EndptId, ClusterId, ClusterImpl)>` where
   `ClusterImpl` is an enum with one variant per cluster-handler type. Flat
   type depth (kills the recursion-limit growth), runtime-sized (N instances
   of any kind), `lifecycle` broadcast + joined `run()` background tasks.
   Endpoints/clusters become runtime-built `Endpoint` values from leaked
   slices (the const-promotion constraint only existed for `clusters!` macros).
3. **Signal layer** (what makes the firmware generic rather than
   per-hardware): a `Source` yields a numeric/boolean reading; a `Sink`
   accepts a command. Sources: built-in firmware signals (SoC temperature,
   motion, tamper, AI rule/class events, radar zones), Linux sysfs (covers
   IIO/hwmon/1-Wire/thermal: BME280, SHT3x, BH1750, DS18B20... with zero
   per-chip code), GPIO input (edge), and externally pushed values
   (HTTP/MQTT, so ANY gateway/PLC/script can feed a Matter sensor);
   later: southbound tags. Sinks: GPIO output, MQTT publish, webhook, signal
   write, or `virtual`. Matter handlers poll/subscribe, then report changes to
   subscribers (`notify_attr_changed`) and emit events.
4. rs-matter 0.3.0 -> 0.4.1 upgrade (spiked in a scratch copy 2026-10-01:
   57 errors, ALL from two mechanical causes — `EpClMatcher` replaced by
   closure matchers, and `rand_core` 0.6 -> 0.10; no logic changes). Done as
   part of the router work since it touches the same lines.
5. Dependency choice for HVAC/energy: time-boxed spike to compile against git
   `main` at a pinned rev to get upstream's conformance-checked Thermostat/Fan/
   ElectricalSensor handlers; if clean, pin the rev, otherwise implement
   minimal typed-layer versions and swap when the next crates.io release lands.

**Phases (each ends with: check + clippy -D warnings + tests + a real `cargo
build` + live boot matrix; hardware regression on the Radxa before release):**
- 19g.0 spikes (done above) + upgrade to 0.4.1.
- 19g.1 registry/router/signal foundation + legacy-config compatibility.
- 19g.2 sensors & switches: Temperature, Humidity, Pressure, Flow,
  Illuminance, Soil, AirQuality + concentration clusters, BooleanState,
  OccupancySensing, Generic Switch (GPIO button, events), AmbientContext/
  AVAnalysis from the AI engine.
- 19g.3 HVAC: Thermostat (typed or upstream B1), Fan, Humidistat; Room-AC /
  air-purifier / heat-pump compositions; Thermostat Suggestions.
- 19g.4 Closures & access: Door Lock, Window Covering, ClosureControl/Dimension.
- 19g.5 Energy: Electrical Sensor (power/energy/topology) from signals.
- 19g.6 Appliances & media: Mode/ModeSelect-derived appliance clusters,
  OperationalState, media/casting endpoints — VIRTUAL-ONLY (no appliance or
  display exists), opt-in with `virtual = true` required and loudly labelled,
  default off. Protocol-complete test endpoints, not product features.
- 19g.7 Node level: configurable BasicInformation/vendor+product IDs and
  attestation inputs; per-endpoint names (optionally bridge/aggregator
  topology with BridgedDeviceBasicInformation so each sub-device is its own
  named accessory in Apple Home); Matter factory-reset + open-commissioning-
  window commands.

**Known issue to fix in 19g.7 (probable root cause identified, unverified):**
the earlier "deleted `config/matter_state/k_*` and restarted, same fabrics came
back" mystery is almost certainly because the files were deleted WHILE the
service was running — the process kept the fabrics in memory and re-flushed
them on shutdown/restart. Correct procedure is stop -> delete -> start; the fix
is a built-in reset command that does it safely.

**Verification gap to close:** no real Matter controller has exercised the
hand-written thermostat. Options: Apple Home on the Radxa; or
python-matter-server (Home Assistant's controller) in Docker on a Linux host.

### 19g.8 — Data-source audit findings (2026-10-01) that shape the signal layer

An audit of what the firmware can already ingest/observe (never fabricated)
found these facts and defects:

**Bugs found and FIXED this session**
- [x] **Modbus TCP was never compiled in**, despite README/TODO/`edge.toml`
      saying so (`iiotedge-protocols` was built with `default-features =
      false` and only `mqtt`/`serial`/`canbus`); a `[[southbound.modbus]]`
      block parsed and then silently produced nothing. Enabled the `modbus`
      feature (`tokio-modbus`, **tcp only — still no Modbus RTU/RS-485**).
      OPC UA stays off (nothing claims it). *aarch64 cross-check pending.*
- [x] **Home Assistant tamper entity was last-transition-wins**: five tamper
      kinds, one binary_sensor, published per kind — blackout clearing while
      occlusion was still alarmed told HA "OFF". Now publishes the aggregate
      (`detector.any_active()`), only on change.

**Open bugs / limitations (tracked, not yet fixed)**
- [ ] Southbound SDK drivers are READ-ONLY (`SouthboundDriver` has no write
      path): a Matter actuator cannot write a Modbus coil/register or CAN
      frame. Sinks are limited to GPIO / MQTT publish / webhook / commands.
- [ ] `[[mqtt_bridge]]` (Zigbee2MQTT / Z-Wave JS / any JSON) feeds ONLY
      `CorrelationProcessor`, and only starts if `[[correlation.rules]]`
      exist (main.rs ~261-271) — its data can't reach any other consumer.
- [ ] No generic "named tag with latest value" abstraction exists anywhere
      (nearest is `OverlayDataBus`: numeric only, only for configured widgets,
      no timestamps/staleness, pull-only). 19g.1 builds one.
- [ ] Radar zones, health/SoC temperature and machine tags are not published
      to Home Assistant at all.
- [ ] `matter::spawn` runs before `tamper_active`/`motion_active`/
      `schedule_armed` are created (main.rs ~559 vs ~820) — they must be
      hoisted (or published into the new signal bus) for Matter to read them.
- [ ] Three independent GPIO implementations (export button, rule pulse,
      Matter relay), no shared helper, no input LEVEL read, no bias config,
      and no line-ownership arbitration (two features on one line silently
      fight). 19g.1 extracts one shared `gpio` module.
- [ ] `gh` has no stored credentials (`gh auth login` needed) so GitHub issues
      can't be listed.
- [ ] Local git history has missing/corrupt old objects (`git log -S` fails
      with "unable to read 8f5fe360…") from the earlier disk-full incident.
      Probably repairable losslessly with `git fetch --refetch` from the
      `fusion` remote if it holds the full history — untried.

**Design refinements adopted from the audit**
- `SignalBus` is created EARLY in `main()` (before `matter::spawn`) and handed
  to everything; existing flags are hoisted/registered as polled sources
  (tamper aggregate `tamper_active`, `motion_active`, `schedule_armed`,
  `Metrics` gauges, `HealthMonitor`), so producers need little or no change.
- A `TagProcessor` (an `iiotedge_core::traits::Processor`, registered where
  correlation/widgets already are, main.rs ~248-276) maps
  `UnifiedPayload.source_id` prefix + `json_field` / byte decode + scale/
  offset + `max_age_s` onto named tags; `mqtt_bridge::deliver` also fans out to
  it. Config: `[[tags]]`. Matter sources then reference `tag:<name>`.
- **Provenance gating ("never fabricate")**: every signal carries a
  provenance (real / virtual / MOCK). The default radar backend is `mock`
  (synthetic track) and every camera on a non-Linux dev host is forced to
  MOCK, so motion/tamper/AI there are synthetic; `[ai].test_hooks_enabled`
  injects detections. A Matter sensor refuses a MOCK-provenance source unless
  an explicit dev flag allows it (and logs loudly), so a real controller is
  never shown synthetic data as if it were real.
- Per-zone motion/AI-rule signals need a hold-timer to give occupancy
  semantics (rule events are momentary pulses; a `presence` rule re-fires on
  every inference frame while an object is in the zone).
- Sensors push live updates (`notify_attr_changed`) and emit events from each
  handler's background `run()`; sources stay pull/poll-based (no producer
  changes needed).

### 19g.2c — Camera-AI-derived Matter sensors (designed + built 2026-10-01; see 19g.9 for what shipped)

The point of generic firmware that is ALSO a camera: a person/vehicle detection
should show up in Apple Home / Google Home as an occupancy sensor with no
glue code. All the pieces exist; what is missing is a source and a hold time.

- **Source scheme `ai:`** — `ai:class:<label>` (a detection of `person`, `car`,
  ... within the last window), `ai:rule:<name>` (the named `[[ai.rules]]` rule
  matched within the window), `ai:any`. A `PulseTable` (last-hit `Instant` per
  key + an `alive` flag) is fed from the analytics loop right where
  `engine.run_inference` returns and where `rule_engine.evaluate` yields events
  (`main.rs`, one map insert per detection per frame).
- **Honesty:** `alive` is only set once the inference engine really started, and
  cleared if it is `None`/failed -> the source reads `None` (Matter null /
  unavailable), NOT "false" (a dead model would otherwise report "nobody there"
  forever). `ai:` sources are refused at config time when `ai.enabled = false`
  (decided in `main()` from config before Matter resolves sources, so there is no
  boot race with the analytics thread). Provenance is Mock on the mock camera, so
  the existing `allow_mock` gate keeps synthetic detections away from real
  controllers.
- **`hold_ms` on `occupancy_sensor`** (`HoldSource` wrapper, generic: also fixes a
  flickery PIR): stays true for N ms after the last true reading. Rule events
  are momentary pulses (a `presence` rule re-fires every inference frame while
  an object is in the zone), so without a hold a Matter occupancy sensor would
  flap at the inference rate.
- **Then** map the same signals onto Matter 1.5's Ambient Context Sensing
  (typed cluster present in 0.4.1) for controllers that understand it; plain
  Occupancy (VISION technology, already shipped) is the interoperable fallback.
- Live harness test: the AI engine isn't part of the harness boot, so cover
  parsing/refusal (an `ai:` endpoint with AI disabled must be absent, not
  "false") and unit-test the table, window, hold and `alive` handling.

### 19g.9 — Progress log (2026-10-01)

**Shipped (each verified: clippy -D warnings, tests, real `cargo build`, live boot,
and — from the router onward — an independent Matter controller):**
- [x] **19g.0** rs-matter capability claims corrected everywhere (`fbceb7e`);
      **upgraded 0.3.0 -> 0.4.1** (`f8360b8`): 57 compile errors from two
      mechanical causes (matcher API, rand_core 0.10). Checked against the
      Radxa's REAL 0.3.0-written fabric state: the new build loads it
      ("Loaded fabric 1 ... already commissioned") — the upgrade does not
      un-pair Apple Home. Manual pairing code now prints 4-3-4 (spec format).
- [x] **19g.1 registry + flat router** (`9a9ca51`): `#![recursion_limit]` bump
      removed and a full `cargo build` still passes; only ENABLED device types
      are constructed (a disabled relay no longer touches its GPIO); endpoint
      ids allocated before handlers are built (a sensor needs its own id to
      notify subscribers); legacy ids 1-4 reserved so paired controllers are
      unaffected; Matter Core spec 9.5 TagLists for endpoints sharing a device
      type + a stable UniqueID for dynamic endpoints.
- [x] **Real-controller harness** `tests/matter-controller/` (matter.js,
      `make matter-verify`, `3bcb849`): commissions over IP and exercises every
      endpoint; **68/68 checks**, including the hand-written Thermostat's
      multi-field `SetpointRaiseLower` decode, constraint rejection and change
      reports — closing the verification gap 19f flagged. matter.js gotchas
      baked in: reads need `requestFromRemote = true` (else it answers from its
      subscription cache and a stale value looks like a firmware bug), and a
      per-attribute error status surfaces as `undefined`, not an exception.
- [x] **19g.2 (first slice) signal layer + config-driven sensors**:
      `src/signals.rs` (builtin / sysfs / gpio_in / push sources, provenance,
      never-fabricated), `[[matter.endpoints]]` (validated, 16 new unit tests),
      `GET /signals` + `POST /signals/<name>` (bearer-gated, 1 KiB body cap),
      `src/matter/sensors.rs`: temperature / humidity / pressure / flow /
      illuminance / occupancy (pir, ultrasonic, physical_contact, **vision,
      radar** — Matter 1.5 technologies) / contact, each with the Identify
      cluster the spec mandates for every sensor device type (checked against
      the spec model, not assumed). No reading -> Matter null (or an
      "unavailable" status for non-nullable attributes); out-of-range -> null;
      mock-camera/radar sources refused unless `allow_mock`.
- [x] **Actuators — the write side (19g.2 "actuator side" + the Fan of 19g.3)**:
      `src/matter/actuators.rs`, `src/gpio.rs`, `sink =` on `[[matter.endpoints]]`.
      `on_off_light` (0x0100, OnOff + LIGHTING), `on_off_plug` (0x010A, plain
      OnOff — advertises none of the LIGHTING-only attributes/commands) and
      `fan` (0x002B, FanControl on rs-matter's TYPED layer), each bound to a
      sink: `gpio:<chip>:<line>[:active_low]` (persistent output, requested
      already driven off), `signal:<name>` (the commanded state shows up in
      `GET /signals` as `push:<name>` for anything else on the device to act
      on) or `virtual` (must be asked for — an actuator with no sink is a
      config error). One device can be several at once: a light and a fan on one
      board are two entries. Honesty rules (unit-tested): the reported state
      moves only if the sink ACCEPTED the command; a fan offers only the speeds
      it really has (`fan_speeds` = off_high / off_low_high / off_low_med_high —
      one GPIO line is one speed, enforced at config validation); no Auto/Smart/
      null-percent (nothing implements an automatic policy); every actuator
      starts OFF and tells its sink so. FanControl's typed setters notify
      NOTHING by themselves — a `FanMode` write must report `PercentSetting` and
      `PercentCurrent` too, which the harness proves from a subscribed
      controller's cache. **120/120 real-controller checks** at the time (was 68; 123 after the legacy Identify below) covering
      boot state, command -> sink, change reports, band mapping (33/66/100),
      refused writes leaving state untouched, TagLists for two lights/two fans,
      LIGHTING vs plain attribute lists. Shared `identify_cluster` moved to the
      registry. Harness config renamed `sensors.toml` -> `endpoints.toml`.
- [x] **Generic Switch + the first Matter EVENTS** (`src/matter/generic_switch.rs`):
      `generic_switch` on any boolean source, `switch_mode = momentary | latching`.
      Momentary = features MS+MSR+MSL+MSM: InitialPress, ShortRelease /
      LongPress+LongRelease, MultiPressOngoing / MultiPressComplete (double and
      triple press); latching = LS: SwitchLatched. The press logic is a pure,
      deterministic-time port of matter.js's `SwitchServer` (the spec behaviour,
      conformance-tested upstream), so every sequence is unit-tested without
      sleeps. Debounce (`debounce_ms`) uses the time an edge FIRST appeared:
      the unit test for "a 790 ms press must not become a long press" caught a
      real bug (a long-press timer firing inside the debounce window), fixed by
      not letting timers run past a still-pending edge; a gap in the readings
      voids a pending edge rather than back-dating it. Honest: no reading = no
      events; a latching switch's CurrentPosition is an error status until it
      has one and its first reading is adopted silently; a momentary switch
      rests at 0. Per-kind poll default (switch 20 ms, sensors 1000 ms;
      `poll_ms` is now optional). The event path was proven the way a real
      controller experiences it — events DELIVERED over its subscription
      (matter.js's own read-back getters skip events the subscription already
      handed over, which looked like missing events until the harness collected
      them properly). **141/141 real-controller checks.**
- [x] **Sensor catalog + change events** (`sensors.rs`): `water_leak_sensor`
      (0x0043), `rain_sensor` (0x0044), `water_freeze_sensor` (0x0041) — the
      BooleanState device types, true = detected — and `soil_moisture_sensor`
      (0x0045, Matter 1.5 SoilMeasurement incl. its MeasurementLimits struct;
      no accuracy figures are invented, the single accuracy range carries
      none). BooleanState gains the CHANGE_EVENT feature + `StateChange`, and
      Occupancy Sensing the OCCUPANCY_EVENT feature + `OccupancyChanged` (the
      1.4/1.5 revisions), emitted from the sensor's `watch` loop via a new
      `watch_with` on-change hook; no event for "unavailable". **176/176
      real-controller checks.** Not done (needs a different config shape —
      one endpoint, several sources): Air Quality Sensor (0x002C) with the
      concentration clusters (CO2, PM2.5, TVOC, ...) and the AirQuality enum
      derived from them; the Occupancy `HoldTime` attribute (see 19g.2c).
- [x] **Camera-AI-derived sensors (19g.2c)**: `ai:class:<label>` /
      `ai:rule:<name>` / `ai:any` sources fed by a `PulseTable` the analytics
      thread updates (one map insert per detection per frame, at the
      `run_inference` result and the `rule_engine.evaluate` result), plus
      `hold_ms` on occupancy sensors (`Hold`, applied after `invert`). Honesty:
      `alive` is set only when the engine really started -> otherwise the source
      reads None (Matter unavailable), not "no one"; `ai:` is refused when
      `[ai].enabled = false` (decided in `main()` before Matter resolves sources,
      no boot race); detections follow the camera's provenance (Mock on the mock
      camera -> `allow_mock` gate). `GET /signals` lists detections seen so far.
      Live-verified in the harness (which boots with AI configured but no model):
      the engine-dead sensor is unavailable, a synthetic source without
      `allow_mock` is absent, the hold keeps occupied for 1.5 s then reports
      the clear. **182/182 real-controller checks.** Still to do here: the
      Occupancy `HoldTime` attribute (controller-adjustable, spec-native) and
      Matter 1.5 Ambient Context Sensing on top of the same signals; the AI path
      itself (a real model on the Radxa) is only unit-tested — see the release
      gate.
- [x] **Node identity (first part of 19g.7)**: `[matter].vendor_name` /
      `product_name` / `device_name` (validated, <= 32 bytes); the product
      name and the mDNS device type are DERIVED from what the node exposes
      (camera on -> "fusion-firmware Camera" + 0x0142, exactly as before so
      paired cameras are unaffected; otherwise "fusion-firmware" + the first
      legacy device's / endpoint's type — a light switch no longer advertises
      itself as a camera); BasicInformation `SoftwareVersion(String)` is this
      build's Cargo version instead of the placeholder `1`. One
      `MatterEndpointKind::device_type()` table is now the single source of truth
      for the endpoint builders AND the advertised type (a test asserts no two
      kinds share an id). Pure `identity()` is unit-tested; harness checks vendor,
      derived product name and software version in the full AND the no-camera
      configuration. **185/185.** Still open in 19g.7: configurable vendor/product
      IDs + attestation cert paths (needs a real certificate), an
      open-commissioning-window command, a safe factory-reset.
- [x] Bugs fixed along the way: Modbus TCP never compiled in (docs said it
      was); Home Assistant tamper entity was last-transition-wins.

**Still open in 19g.2:**
- [ ] An edge-driven (gpio-cdev line events) source for sub-millisecond latency
      if 20 ms polling ever proves too coarse; the ACTION_SWITCH feature
      (multi-position selectors) if a use appears.
- [ ] More clusters on the same pattern: SoilMeasurement (1.5), AirQuality and
      the concentration clusters, ElectricalPower/EnergyMeasurement.
- [ ] More built-in signals: per-class AI detections (`ai:<class>`), radar zone
      occupancy (needs a shared state handle + a hold-timer — rule events are
      momentary pulses), cluster peer count.
- [ ] `[[tags]]` + a `TagProcessor` so southbound Modbus/serial/CAN values and
      `[[mqtt_bridge]]` JSON become named signals (the SDK has no tag store, and
      mqtt_bridge feeds only the correlation processor today).
- [ ] More sinks: `mqtt:<topic>` / `webhook:<url>` so a non-GPIO load (a smart
      plug on another bus, a PLC register) can be commanded without a gateway
      script polling `GET /signals`; and the closures/lock/cover actuators
      (19g.4) on the same sink layer. A PWM/dimmer sink would also let
      `[matter.light]` drive real hardware.
- [ ] **Conformance gaps on the actuator endpoints** (controllers work today;
      tracked so they aren't forgotten): Groups (all of light/plug/fan) and
      Scenes Management (light/plug) are MANDATORY on those device types.
      rs-matter's Groups needs its `groups` feature, which pulls in multicast
      Groupcast and does not build on the macOS dev host — gate it
      `cfg(target_os = "linux")` or wait for upstream. The OLDER relay
      (`[matter.onoff]`, endpoint 2) advertises rs-matter's FULL OnOff cluster —
      the LIGHTING-only attributes/commands included — without claiming the
      LIGHTING feature; switching it to the plug metadata is a one-liner but it
      is paired in Apple Home, so do it with the on-device regression pass.
      Likewise the old relay caches the REQUESTED state even if the GPIO write
      failed (the new endpoints don't).
      (Identify, also mandatory there, is now present on the legacy light,
      relay and thermostat endpoints too — purely additive, 123/123 checks.)
- [ ] Verified on macOS + aarch64 type-check/clippy only; **not yet deployed to
      the Radxa** — hardware regression (camera + relay still paired in Apple
      Home) is the gate before release.

**Next phases unchanged:** 19g.3 HVAC (Fan; Thermostat onto the typed layer or
upstream's `ThermostatHooks` once released), 19g.4 closures/locks, 19g.5 energy,
19g.6 appliances/media (virtual-only, opt-in), 19g.7 node-level settings.

## Phase 20 — Remote AI/automation config (MQTT + HTTP) with
## restart-persistence — DONE 2026-08-06, requested by Santosh: "do we
## have any endpoint from where i can config and change ai relategt or
## automzatoin related over server e.g. mqtt or etc way? ... also persist
## config so that next time don't pick from config file direcly if
## device restart"

Before this phase, `[[ai.rules]]` (Phase 16) was config-file-only: change
a rule, edit the TOML, restart the device. This adds a read/write remote
endpoint over BOTH channels the ask named — MQTT (reusing the existing
command channel) and HTTP (`e.g. ... or etc`) — plus the persistence half
of the ask: a remotely-applied rule set survives a reboot instead of
reverting to whatever's baked into the shipped config file.

- [x] **`config_get_ai_rules` / `config_set_ai_rules`** MQTT commands
      (`src/commands.rs`) alongside the existing `config_get` (which stays
      as-is, whole-file, read-only). `config_set_ai_rules` REPLACES the
      entire `[[ai.rules]]` list (not a merge/patch) — same semantics as
      hand-editing the array in the TOML file. Gated by the same
      `[security].command_token` bearer check every other command already
      uses (empty token = unauthenticated, the existing "empty means
      open" convention) — this doesn't create a new trust boundary, it
      extends an already-high-trust credential (which can already
      `reboot`/`export`/`stream_start`) to one more thing.
- [x] **`GET`/`POST /config/ai-rules`** on the existing metrics HTTP
      server (`src/core/metrics.rs`, same port as `/metrics`/`/footprint`/
      `/onboarding/*`) — the "or etc" half of the ask, for anything that
      isn't an MQTT client (curl, a browser, a simple integration). Same
      bearer-token gate, same underlying validate/persist/apply function
      as the MQTT path, so neither channel can accept something the other
      would reject.
- [x] **Restart-persistence** (`src/runtime_config.rs`, new module):
      `[system].ai_rules_override_file` (default
      `config/ai_rules_override.json`) holds the last remotely-applied
      rule set as JSON. `runtime_config::resolve()` runs at boot right
      after identity resolution and, if a valid override exists, REPLACES
      the static file's own `ai.rules` for that run — same "persist to
      disk, re-read at next boot" pattern `src/identity.rs` already
      established for device identity, applied to a richer payload. A
      missing, corrupt, or now-invalid override file just means the TOML
      file's own `ai.rules` wins, with a WARN explaining why — a bad
      remote push must never brick the device.
- [x] **Live application without a restart**: the analytics thread
      (`main.rs`) owns `RuleEngine` as a plain `&mut`-owned value, not
      `Mutex`-shared state like `PtzController` — a deliberate difference
      already established this session. A remote update hands off through
      a new `RuleUpdateSlot` (`Arc<Mutex<Option<Vec<AiRule>>>>`) instead:
      the command/HTTP handler sets it, the analytics thread polls it once
      per frame and rebuilds `RuleEngine`. Verified end-to-end against the
      mock camera: POST → live rebuild log line → restart → override
      re-applied log line, all real, not just unit-tested.
      Known/accepted limitation: a brand-new rule's Home Assistant
      discovery topic (Phase 19a) doesn't appear until next reboot (HA
      discovery publishes once at boot off the rule-name list at that
      time) — editing an existing rule still reflects live, since that
      reuses its already-discovered topic. Full dynamic HA re-discovery
      wasn't asked for and is out of scope here.
- [x] **Validation reuse, not duplication**: the per-rule checks that used
      to live inline in boot-time `validate()` were extracted into
      `pub fn validate_ai_rules()` (`config.rs`) — the exact same function
      both a fresh boot and a remote change run through, so nothing
      settable over the network is ever more permissive than a config
      file would already allow.
- [x] **Adversarial review** (a fresh independent agent, briefed cold on
      the diff, asked to find real exploitable issues rather than recite a
      checklist) found two real problems, both fixed and covered by new
      tests before this was called done:
      - Unbounded rule count / string lengths / HTTP body size — a
        network-reachable endpoint (unauthenticated whenever
        `command_token` is left empty, an existing documented mode) with
        no caps could bloat disk/memory/boot time, and an oversized POST
        body could stall the metrics server's other routes (it's
        single-threaded). Fixed: `MAX_AI_RULES`/`MAX_ZONE_POINTS`/string-
        length caps in `validate_ai_rules`, and a 256 KiB hard cap on the
        HTTP body via a bounded `Read::take()` (not just trusting
        `Content-Length`, which a client can lie about or omit).
      - Every remote update — even re-submitting the exact same rule set
        unchanged — rebuilt `RuleEngine` from scratch, silently resetting
        every in-flight loiter dwell timer and line-cross side-state
        (`ai/rules.rs`'s `CompiledRule.tracks` has no persistence across a
        rebuild). A fleet config-sync loop reconciling to the same desired
        state on a schedule, or simply polling the endpoint, could
        suppress loitering alerts by resetting timers faster than
        `dwell_s`, with zero errors logged. Fixed: `apply_and_persist` now
        compares the incoming rules against what's currently in effect
        (`AiRule`/`ScheduleConfig`/`ScheduleWindow` gained `PartialEq` for
        this) and skips both the disk write and the live handoff on an
        identical resubmission — a genuine no-op, not treated as an error.
      - (Lower severity, fixed alongside the above) two near-simultaneous
        callers on different channels (MQTT + HTTP) could interleave their
        persist-then-apply sequences; `apply_and_persist` now holds
        `RuleUpdateSlot`'s own lock across the whole check-persist-apply
        decision, not just the final handoff.
- [x] 9 new unit tests (`runtime_config.rs`) + full manual smoke test
      against the mock camera (auth reject/accept, invalid-rule 422,
      valid-rule apply, no-op-resubmission skip, oversized-body 413,
      restart-persistence). 137/137 tests passing, clippy clean.
