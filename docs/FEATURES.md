# Fusion Firmware — Production Feature Specification

Target: production-ready, mass-deployable edge vision firmware for Industry 4.0 — not
just a camera driver: on-device AI, cross-device cluster mesh, cloud relay, QR
onboarding, and telemetry with southbound machine-data ingestion, all in one binary.
SDK policy: **consume `iiotedge-lib` as-is** (telemetry engine, store-and-forward buffer,
TLS/TPM security, southbound machine drivers). The lib is never modified from this repo.

---

## 1. Current state (baseline, v1.0.0)

What exists and is kept (nothing is removed, only refactored/extended). This table describes
the shipped v1.0.0 release, not the original day-0 skeleton this doc started from — see
[RELEASE_NOTES.md](../RELEASE_NOTES.md) and the README feature table for the user-facing view:

| Area | File(s) | Status |
|---|---|---|
| Multi-thread pipeline (capture → AI / encoder) w/ CPU pinning | `src/main.rs` | Working |
| Zero-copy frame router (bounded, drop-on-full) | `src/core/ring_buffer.rs` | Working |
| DMA buffer abstraction | `src/core/memory.rs` | Working (unused re-queue hook) |
| HAL trait + registry-based factory (`VideoSource`) | `src/hal/mod.rs` | Working — pluggable backend registry (`register_source`), replaces the old hardcoded `match` |
| Mock camera (macOS dev / CI, no hardware) | `src/hal/mock_cam.rs` | Working |
| Generic V4L2 camera (mmap ioctls, UVC) | `src/hal/generic_v4l2.rs` | Working (Linux-only) |
| NXP i.MX8MP ISP camera | `src/hal/nxp_isp.rs` | Working (Linux-only) |
| USB camera | `src/hal/generic_usb.rs` | Empty stub — not yet started |
| Hardware encoder | `src/stream/encoder.rs` | Working — auto-probed Rockchip MPP / NXP VPU / V4L2 stateful / VideoToolbox / software fallback |
| RTSP server | `src/stream/rtsp_server.rs` | Working — `gstreamer-rtsp-server`, shared pipeline |
| MQTT command channel | `src/commands.rs` | Working — `iiotedge/<group>/<node>/cmd` in, `.../cmd/ack` out, 10-command v1 set |
| Metrics | `src/core/metrics.rs` | Working — Prometheus counters/gauges |
| AI engine | `src/ai/engine.rs`, `src/ai/runtime.rs`, `src/ai/backends/{onnx,rknn}.rs` | Working — ONNX Runtime and RKNN (Rockchip NPU) backends, config-selected |
| TOML config | `src/config.rs`, `config/iiotedge_default.toml` | Working |
| Cross build | `Dockerfile.cross`, `Makefile` | Working — `make docker-release` / `docker-check` / `docker-lint` |

`iiotedge-lib` is consumed as a path dependency (telemetry engine, store-and-forward buffer,
TLS/TPM security, southbound machine drivers) per the SDK policy noted above.

---

## 2. Feature specification

### F1 — Streaming (RTSP + switchable H.264/H.265)
- RTSP server (`gstreamer-rtsp-server`), TCP+UDP, per-stream mount points.
- Codec switchable **per stream profile via config**: `h264` | `h265`.
- Encoder backend selection via config-driven candidate lists probed against the
  device's GStreamer registry: Rockchip MPP (`mpph264enc`/`mpph265enc` —
  **Radxa Zero 3E / RK3566, the reference target**; 1080p encode ceiling),
  NXP VPU (`vpuenc_h264`/`vpuenc_hevc` — i.MX8MP), V4L2 stateful
  (`v4l2h264enc`), Apple VideoToolbox (dev Macs), software fallback
  (`x264enc`/`x265enc`). All element knowledge confined to `stream::encoder`.
- Multiple profiles: `main` (e.g. 4K@30 H.265) + `sub` (e.g. 720p@15 H.264) + `ml` tap.
- Tunables per profile: resolution, fps, bitrate (CBR/VBR), GOP, B-frames, profile/level.
- RTSP authentication (basic/digest, users from config/provisioning).
- RTCP sender reports carry the same clock used for event correlation (F7).
- **Audio** (`[audio]`, off by default, shipped 2026-07-16): one capture pipeline
  (`src/audio.rs`) feeds both consumers from the same samples — RTP audio track on
  the RTSP stream and an audio track in NVR chunk recordings. Source is
  config-driven (`"auto"` → ALSA/CoreAudio, or any explicit GStreamer source
  fragment for a specific device/bench tone); codec is opus (built-in) or
  aac (encoder probed at runtime), same auto-probe pattern as video encoders.
  Audio failures never touch the video path. Pending: ONVIF audio source
  declaration, per-device gain control.
- **Cloud-push relay** (`[cloud_relay]`, on by default, shipped 2026-07-19,
  live-verified 2026-07-25, see TODO.md's Phase 3 entries for full as-built
  detail): the local RTSP server above stays LAN-only pull always; on the
  `stream_start`/`stream_stop` MQTT commands (`src/commands.rs`), a
  separate in-process GStreamer pipeline (`src/stream/relay.rs`) pulls that
  same feed and republishes it, unmodified, to media-ingestion-service's
  MediaMTX for remote viewing. Auto-reconnects with exponential backoff on
  its own (2026-08-06) instead of requiring an external re-`stream_start`
  round-trip — the single biggest latency cost in WAN recovery time.
  `stream_start`'s `mode` field picks the transport per call: `"rtsp"`
  (default, `rtspclientsink`) for VMS/ONVIF-style consumers, or `"webrtc"`
  (`whipclientsink`, WHIP ingest, 2026-08-06) for the low-latency live-view
  path — UDP/SRTP with NACK/FEC/congestion control and sub-second
  ICE-restart reconnects, the same transport class Hikvision/Verkada/Ring/
  Nest-tier live view uses instead of RTSP-over-TCP's WAN-hostile
  head-of-line blocking. Both modes coexist (chosen per call, not a device-
  wide switch) and share the same auto-reconnect machinery. WebRTC mode not
  yet live-verified against a real MediaMTX WHIP endpoint (only the
  pipeline construction itself, against the real `whipclientsink` element)
  — same gap the RTSP path had before its own 2026-07-25 live verification.

### F2 — ONVIF conformance (Profile S baseline, Profile T stretch)
- WS-Discovery responder (device discoverable by VMS/NVRs).
- SOAP services: Device Management (info, capabilities, time, reboot, users),
  Media (profiles, stream URIs), Events (tamper/AI events via WS-BaseNotification pull point).
- Profile T items behind a feature flag: H.265 media2 service, metadata streaming.
- ONVIF user auth (WS-UsernameToken) backed by the same user store as RTSP.
- **PTZ** (`[ptz]`, off by default — most deployments are fixed cameras): ONVIF PTZ
  service (ContinuousMove/Stop/SetPreset/GotoPreset/GetPresets, advertised in
  GetCapabilities/GetProfiles only when enabled) and MQTT commands
  (`ptz_move`/`ptz_stop`/`ptz_preset`) both dispatch through one shared
  `PtzController` — never two independent control paths racing each other. Backend
  is a registry (`src/ptz/`, mirrors the camera HAL): Pelco-D over RS-485/RS-232
  today, ONVIF passthrough (for `rtsp_in`-proxied cameras) once that HAL backend
  exists. A safety watchdog auto-stops a ContinuousMove that never gets a Stop.

### F3 — Local storage (edge recording)
- Segmented circular recording (fMP4/MKV segments, configurable duration, e.g. 10 s)
  onto SD/eMMC/NVMe with retention by size + age; filesystem-full watermark handling.
- Event-triggered clips: pre-roll/post-roll around AI, tamper, and machine-correlated
  events (fed from the always-on segment ring).
- Recording index (segments, events, clip metadata) in a local SQLite DB owned by the
  firmware (separate from `iiotedge-storage`'s telemetry buffer, same durability idea).
- Snapshot (JPEG) capture on demand and on event.
- Telemetry store-and-forward: **`iiotedge_storage::SqliteBuffer`** — every event is
  persisted before any network attempt (SDK persist-first pipeline).

### F4 — Tamper detection
- Video analytics tampers: occlusion/blackout, defocus/blur, scene change
  (camera moved/repointed), excessive brightness (flashlight attack), signal loss/freeze.
- Physical tampers: enclosure-open GPIO, IMU orientation change (when hardware exposes it).
- Debounce + severity + arming schedule in config; events go to: ONVIF Events,
  MQTT/Sparkplug (via lib engine), local event log, optional OSD banner, SNMP trap.

### F5 — ML / AI pipeline
- Real ONNX Runtime (`ort`) inference engine with execution-provider selection from
  config: `NPU`/`GPU`/`CPU` delegates, graceful fallback.
- Pluggable pre/post-processors (YOLO-style detection parser first; classification and
  custom parsers registerable).
- ROI, confidence threshold, class filter, inference FPS limit (decoupled from stream FPS).
- Model management: models referenced by id+version+sha256, atomic hot-swap on config
  update / remote command (OTA model delivery rides the fleet channel, F10).
- AI events: normalized schema (label, score, bbox, frame id, **capture timestamp**),
  fan-out to overlays (F6), storage clips (F3), telemetry (F8), cluster bus (F9).

### Phase 16 — Customizable AI detection rules (zones / line-crossing /
### loitering / workflow actions) — 16a/16b/16c implemented 2026-08-04, see
### `src/ai/rules.rs`, `src/ai/actions.rs`, `config.rs`'s `AiRule` and
### TODO.md's "Phase 16" for full as-built detail
- Config-driven `[[ai.rules]]` (array-of-tables, on top of F5's AI event
  stream — doesn't touch `ai/parser.rs`): each rule has a `name`, `classes`
  filter (subset of `ai.labels`), optional `min_confidence` override, a
  `zone` (normalized `[0,1]` points — 2 = line, 3+ = polygon, so one field
  covers both geometries), a `mode`, and an optional per-rule `schedule`
  (reuses `ScheduleConfig`, same as everywhere else in this firmware).
- Three rule modes, matching the vocabulary every mainstream VMS/NVR
  analytics product converges on (Axis Object Analytics, Frigate, ONVIF
  Profile M): **`presence`** (object inside a polygon — ray-casting
  point-in-polygon test), **`line_cross`** (object crosses a 2-point line
  in a configured `direction`), **`loiter`** (object stays inside a polygon
  past `dwell_s`). All three test the detection's **bottom-center** point
  (the standard ground-contact convention, not the bbox centroid).
- Lightweight state tracking without a full multi-object tracker (parser.rs
  is single-frame NMS only): line-crossing keys state by position
  *projected along the line's own direction* so an object's "which side"
  state survives the crossing motion; loitering keys state by **class
  alone** so one object's dwell timer doesn't fragment as it drifts inside
  the zone (accepted tradeoff: multiple same-class objects in one zone can
  confuse it — full SORT/ByteTrack-style tracking would fix that but wasn't
  needed yet).
- A fired rule produces a `RuleEvent` that always publishes `rule_event`
  telemetry, and can additionally trigger any of: `snapshot`, `clip`
  (reuses `ClipExtractor`), `cluster_broadcast` (reuses F9's fusion bus),
  `webhook` (HTTP POST via a dedicated non-blocking dispatcher thread —
  `ai/actions.rs`'s `WebhookDispatcher`, mirrors the clip extractor's
  bounded-channel shape so a slow endpoint can't stall analytics),
  `gpio_output` (pulses a configured GPIO line for a siren/relay/light,
  Linux-only via `gpio-cdev`, already a dependency).
- Mobile-app zone-drawing UI is explicitly out of scope for this repo (same
  firmware/app boundary already drawn for F9's cluster mesh and Phase 12a's
  QR onboarding) — the normalized `[0,1]` coordinate space is what a "draw
  on the live preview" UI would consume directly, no translation needed.
  ONVIF Profile M analytics-event publishing (third-party VMS consumption)
  and PTZ-preset rule actions are flagged as separate, larger follow-ups,
  not built here.
- **20 production-ready use-case presets** (`config/presets/`, shipped
  2026-08-04, see `config/presets/README.md`): complete, deployable
  configs for real Industry 4.0, target-customer-vertical, and
  residential smart-home scenarios. 10 single-camera: perimeter
  intrusion, restricted machine safety zones, dock loitering, gate
  counting, production-line correlation/widgets HMI, multi-camera
  cluster mesh, after-hours lockdown, PPE compliance, cold-storage
  tamper monitoring, forklift/pedestrian shared lanes. 5
  `cluster-fusion-*` multi-camera deployments built around cross-device
  detection fusion (`cluster/fusion.rs`) for verticals beyond
  factory-floor Industry 4.0: retail loss prevention, critical
  infrastructure/utility perimeter, campus security, construction site,
  smart parking. Plus 5 `home-*` residential scenarios built around
  Phase 19's Home Assistant/Zigbee integration: front door, driveway
  arrival→HA-lighting, garage, pool safety, and a whole-house
  multi-camera mesh capstone. All built entirely on shipped features,
  not the (unbuilt) LiDAR-based Phase 14e list. Every preset is
  regression-tested by
  `config::tests::every_shipped_preset_parses_and_validates`.

### F6 — Industry 4.0 overlays (OSD), customizable
- Overlay engine compositing before encode, fully config-driven layout:
  - timestamp (clock-synced, ms precision), device id / facility / line id,
  - free text, image/logo (PNG), privacy masks,
  - AI bounding boxes + labels, tamper banner,
  - **live machine-data fields** (e.g. `line1/temp = 21.5 °C`) from southbound drivers (F7),
  - diagnostics (fps, bitrate, sync offset) toggleable.
- Per-stream-profile overlay sets; positions in relative coordinates; TTF/Pango font
  config (`textoverlay`/`clockoverlay` GStreamer elements) for the text layer above.
- **Realtime graph/chart widgets** (`[[overlay.widgets]]`, shipped 2026-07-16,
  `src/stream/widgets.rs`) — a second, independent rendering path purpose-built
  for the "process data live on the footage" ask: sparkline trend graphs, bar
  gauges, and big-number values drawn directly into the luma plane from any
  southbound telemetry tag (bound by source-id prefix, optional JSON field
  extraction, auto- or fixed-scale). Dependency-free — a built-in 5×7 bitmap
  font/line/box renderer, not Pango, since GStreamer's text elements can't draw
  graphs. Renders identically into the live RTSP stream and NVR recordings, same
  as the text overlays above. Pending: color/theming, more widget kinds.
- Non-burned alternative: ONVIF metadata stream / MQTT sidecar for VMS-side rendering.

### F7 — Machine-data ↔ video time correlation
- Clock discipline: PTP (IEEE 1588) preferred, NTP fallback; sync status exposed as
  metric + health state; hardware capture timestamps (`timestamp_ns`) carried
  end-to-end (capture → AI event → encoder PTS → RTCP SR → recording index).
- Machine data in: **`iiotedge-protocols` southbound drivers** (Modbus TCP, OPC UA,
  serial, CAN/J1939 — whatever the site needs) via `build_drivers(&config)`.
- Correlation engine: tag rules (e.g. `trigger: opcua://plc1/reject_flag == true`)
  match machine samples to frames within a configurable tolerance window (± ms);
  emits a **correlated event** = {machine payload, frame timestamp, stream offset,
  clip reference, snapshot} so text data and video line up frame-accurately.
- Machine trigger → camera actions: snapshot, clip, overlay flash, AI run-on-demand.

### Phase 19 — Smart-home ecosystem integration (Home Assistant / Zigbee /
### Z-Wave) — 19a/19b implemented 2026-08-04, see `src/homeassistant.rs`,
### `src/mqtt_bridge.rs` and TODO.md's "Phase 19" for full as-built detail;
### 19c (Matter) stays design-only, see TODO.md
- **Home Assistant MQTT Discovery** (`[home_assistant]`, off by default):
  publishes retained `<discovery_prefix>/<component>/<device_id>/<object_id>
  /config` messages per the HA spec — a `binary_sensor` per `[[ai.rules]]`
  entry (momentary, auto-resets via `off_delay_s`), one aggregate tamper
  sensor, one motion sensor per configured zone, and Snapshot/Clip buttons.
  Rides the SAME broker/identity as the MQTT command channel (`src/commands.rs`)
  on purpose: a button's `command_topic` is literally the existing command
  topic with the exact JSON `handle_command` already parses, so pressing
  it in HA runs a real command with **zero new command-ingestion code**.
  Two things already worked with no firmware changes at all, documented
  rather than built: HA's Generic Camera/ONVIF integrations already
  consume this firmware's existing RTSP/ONVIF directly, and an
  `[[ai.rules]]` `webhook` action can already POST to an HA automation's
  webhook trigger.
- **Generic MQTT-JSON southbound bridge** (`[[mqtt_bridge]]`, none
  configured by default): subscribes to an existing JSON-over-MQTT source
  — Zigbee2MQTT, Z-Wave JS UI, or any other — and feeds every message into
  the SAME `[[correlation.rules]]` engine above via a direct
  `Processor::process` call (this bridge isn't a registered
  iiotedge-lib southbound driver, so there's no engine ingest path to
  ride; calling the tap directly reuses the exact same rule-matching/
  snapshot/clip logic with zero duplication). `source_id` is the raw MQTT
  topic (e.g. `"zigbee2mqtt/front_door"`), matching `source_prefix` the
  same way `"serial/scanner1"` already does. Deliberately not a native
  Zigbee/Z-Wave radio stack — no mature Rust MAC/PHY crate, no radio
  hardware on the reference device, and these bridges are already the de
  facto standard most Home-Assistant-adjacent sites run (same "don't
  build infrastructure blind" call as Phase 12c/12d).
- **Matter support (19c) is explicitly NOT built** — genuinely
  bleeding-edge: Matter 1.5 (Nov 2025) is the first version with a
  Camera device type at all, and whether the only viable Rust SDK
  (`rs-matter`) has implemented the new Camera/WebRTC Transport clusters
  was unconfirmed as of this research — see TODO.md Phase 19c for the
  full blocker list and re-verification steps before starting.

### Phase 20 — Remote AI/automation config with restart-persistence —
### implemented 2026-08-06, see `src/runtime_config.rs`, `src/commands.rs`,
### `src/core/metrics.rs` and TODO.md's "Phase 20" for full as-built detail
- **Read/write `[[ai.rules]]` over the network**, not just the config
  file: `config_get_ai_rules`/`config_set_ai_rules` MQTT commands
  (`src/commands.rs`, same command channel/bearer-token gate as every
  other command) and `GET`/`POST /config/ai-rules` (`src/core/metrics.rs`,
  same port/auth pattern as `/footprint`/`/onboarding/*`) — both funnel
  through one shared validate-persist-apply function so neither channel
  can accept something the other, or a fresh boot from the TOML file,
  would reject. `config_set_ai_rules`/`POST` REPLACE the whole rule list
  (not a merge/patch).
- **Survives a restart**: `[system].ai_rules_override_file` persists the
  last remotely-applied rule set as JSON, re-applied at boot (right after
  device-identity resolution) in place of the static config file's own
  `ai.rules` — same "persist to disk, re-read at next boot" pattern
  `identity.rs` already established for device identity. A missing,
  corrupt, or now-invalid override file just falls back to the config
  file's rules with a logged warning; a bad remote push can never brick
  the device.
- **Applies live, no restart needed**: a `RuleUpdateSlot` hands the new
  rule set from the command/HTTP thread to the analytics thread (which
  owns `RuleEngine` as a plain `&mut`-owned value, not `Mutex`-shared
  state like `PtzController`), rebuilding it on the next frame.
- **Hardened after an adversarial review** (independent fresh-context
  review, not a self-check): rule count/string-length caps and a 256 KiB
  HTTP body cap (closing a single-threaded-server DoS route), plus an
  idempotency check that skips the disk write and live rebuild entirely
  when a resubmitted rule set is identical to what's already running —
  rebuilding on every call, even a no-op one, would have silently reset
  every in-flight loiter dwell timer / line-cross state, which is exactly
  the kind of thing that could be used to suppress loitering alerts by
  polling the endpoint faster than `dwell_s`.

### F8 — Telemetry northbound (via iiotedge-lib, unmodified)
- `EngineBuilder` + `SqliteBuffer` + `MqttTransport` from the SDK: persist-first,
  FIFO, at-least-once; Sparkplug B (NBIRTH/NDATA/NDEATH) or GDE JSON envelope.
- TLS/mTLS from `iiotedge_security::build_client_tls`; optional TPM 2.0 client key.
- All firmware events (AI, tamper, correlation, health) are `UnifiedPayload`s
  ingested through `EngineHandle::ingest`.
- Command channel (MQTT subscribe): reboot, get/set config, snapshot, start/stop
  recording, model swap, log level — with acks and audit log.

### F9 — Cluster mode (multi-camera coordination) — implemented 2026-07-24,
### see `src/cluster/` and TODO.md's "Phase 11" for full as-built detail
- Cluster bus: **not** MQTT/broker-based (revised from the original spec below) —
  broker-less WiFi UDP multicast (`cluster/wifi.rs`) and/or Bluetooth LE advertising
  (`cluster/bluetooth.rs`, Linux/BlueZ), so coordination keeps working through a
  WAN/broker/internet outage, not just a WAN one. `ClusterTransport` trait keeps both
  swappable; either or both can run at once.
- Peer discovery: periodic `Announce` broadcast (identity/capabilities/priority/health)
  plus an active `Ping`/`Pong` liveness probe on its own jittered schedule — no mDNS/
  WS-Discovery, no static peer list; ±20% jitter (xorshift64) on broadcast intervals
  avoids same-config devices drifting into lockstep.
- Node registry + simple deterministic bully election: highest `(priority, node_id)`
  among peers heard from within a timeout window wins leader — self-healing, no
  election message round-trip.
- Event sharing: AI/tamper/motion/correlated events relay onto the cluster bus
  (`MessageKind::Event`) alongside their normal GDE telemetry publish.
- Cross-device agentic control (rule-based, no LLM): `[[cluster.reactions]]` can also
  send a `MessageKind::Command` to one peer or `"*"` — the receiving device runs it
  through the exact same audited `commands::handle_command` path (and bearer-token
  check) MQTT commands use, gated by `[cluster].accept_remote_commands` (off by
  default). A built-in `reanalyze` command drives peer-triggered AI re-analysis:
  bypasses `ai.inference_fps_limit`'s skip for one frame.
- Cross-device AI detection fusion (`cluster/fusion.rs`): correlates this device's own
  recent AI detections against peers' `Event{kind:"ai_event"}` broadcasts — the same
  object crossing multiple camera FOVs within `[cluster.fusion].tolerance_ms` produces
  one corroborated `fused_detection` telemetry event. No wire protocol change — reuses
  the existing compact per-batch label summary (kept small for BLE's payload budget).
  Distributed inference / task handoff (the remaining "future distributed AI" item) is
  deferred pending a load/capability signal `Announce` doesn't carry yet.
- **`GET /cluster/status`** (`core/metrics.rs`, alongside `/metrics`/`/healthz`, same
  `[system].metrics_port`): the read-only HTTP export point for any external
  consumer — mobile app, dashboard, `curl`, anything — no MQTT/cluster-protocol
  client needed. `{"enabled":false}` when cluster mode is off; otherwise:
  ```json
  {
    "enabled": true,
    "is_leader": true,
    "peer_count": 2,
    "peers": [
      {"node_id": "cam-2", "priority": 100, "healthy": true, "last_seen_ms_ago": 1234}
    ],
    "recent_command_acks": [
      {"source_device": "cam-2", "ack": {"ok": true, "id": "...", "cmd": "snapshot"}}
    ]
  }
  ```
  `peers` only lists devices heard from within the liveness timeout (30s). Reading
  `recent_command_acks` drains it — a best-effort recent-activity snapshot, not a
  durable log; poll more often if that matters. `ack` is always whatever
  `commands::handle_command` produced (so it always carries `cmd`/`id`/`ok`). This
  endpoint carries no peer IP/address — it's a status view, not a control surface;
  cluster coordination itself stays on the UDP multicast/BLE wire protocol above, with
  no HTTP trigger for it.

### Phase 12a — QR device onboarding — implemented 2026-07-24, see
### `src/onboarding.rs`, `core/metrics.rs` and TODO.md's "Phase 12a" for
### full as-built detail; mobile-app-facing contract in `docs/QR_ONBOARDING.md`
- Replaces manual IP/port/credential entry with one scan: `GET /onboarding/qr.png`
  (alongside `/metrics`/`/cluster/status`, same `[system].metrics_port`) renders a
  PNG QR code encoding device_id, LAN host (from the request's Host header — correct
  on multi-homed devices, not a best-effort local-route guess), ONVIF/RTSP/metrics
  ports, RTSP+ONVIF credentials (the shared `[security].users` store), and a bearer
  `api_token` the app should present on future `/cluster/status` calls.
  `GET /onboarding/info` returns the identical payload as plain JSON.
- New auth tier: **`[security].api_token`** — a bearer token distinct from
  `command_token` on purpose. `command_token` stays known only to installers/
  provisioning tooling and can issue commands (MQTT + cluster peer); `api_token` is
  what the QR hands to the *end-user's phone* for read-only HTTP API access, so a
  compromised app can never reach the command channel. `GET /cluster/status` now
  requires `Authorization: Bearer <api_token>` whenever that field is set (empty ⇒
  unauthenticated, same "empty means open" convention every other auth knob in this
  firmware already uses).
- The onboarding routes themselves are gated by the *existing* `command_token` —
  presented as `Authorization: Bearer <token>` or `?token=<token>` (query param
  specifically so an installer can open the QR PNG directly in a browser or `<img>`
  tag, which can't set a header). `[onboarding].enabled` (default `true`) can turn
  both routes off entirely. A device with an empty `command_token` logs a boot
  warning that these routes are reachable by anyone on the LAN.
- Payload schema is versioned (`"schema": "iiotedge.onboarding.v1"`) so the app can
  reject shapes it doesn't recognize rather than guess field meanings; see
  `docs/QR_ONBOARDING.md` for the full JSON contract and scanner-implementation
  guidance for the mobile app team. No mobile-app-repo code was changed — same
  firmware-side-only boundary as F9's Phase 11b.

### F10 — Mass production & fleet (device identity, SNMP, footprint)
- **Device identity** — done: `device_id` derived from hardware when
  `[system].device_id` is left empty (device-tree serial-number → primary NIC MAC →
  random UUID), persisted to `[system].identity_file` so it survives reboots
  (`src/identity.rs`). TPM-backed identity not built — no TPM assumed on the
  reference hardware.
- **Device footprint** — done: `GET /footprint` + a one-time GDE `device_birth`
  telemetry event (model, firmware version + build-time git hash, config-file
  sha256, enabled-feature list) — `src/footprint.rs`, `build.rs`.
- **SNMP agent** — done, v2c only (v3's USM auth/privacy not built — separate,
  real complexity). Hand-rolled BER/ASN.1 (`src/snmp/`), verified against real
  `snmpget`/`snmpwalk`, not just self-round-tripped. MIB-II system group + a
  private enterprise MIB backed by the same counters `/metrics` already serves.
  Tamper-alarm trap wired.
- **systemd `sd_notify` + packaging** — done: `WATCHDOG=1` tied to the exact same
  liveness check that drives the firmware's own exit(2)-on-stall (`core/sd_notify.rs`),
  catching a failure mode the internal watchdog alone can't (the main capture
  thread blocking forever). `.deb` packaging via `make deb`. Yocto recipe not
  built — needs a real Yocto build environment to develop against.
- **Provisioning + certificate enrollment** — design-only (TODO.md Phase 12c):
  first-boot CSR enrollment against a CA/provisioning server that doesn't exist
  yet for this fleet. Deliberately not built blind against an invented protocol;
  the server-side choice (step-ca vs. a custom minimal REST endpoint) has to
  happen first.
- **OTA A/B updates** — design-only (TODO.md Phase 12d): blocked on a real
  decision this repo can't make alone — the reference OS image (stock
  Radxa/Armbian Debian) has no A/B partitioning today, so the partition-layout
  and RAUC-vs-swupdate choice has to be made before any firmware-side
  `ota_update` command is real rather than theoretical.

### F11 — HAL v2 (any camera, flexible & extendable)
- Registry-based factory (`register_source("VENDOR_X", ctor)`) — **done**, see `src/hal/mod.rs`.
  New cameras are added by implementing `VideoSource` and calling `register_source()` once;
  `create_camera()`'s dispatch logic never changes. `mock`, `generic_v4l2` (mmap + DMABUF),
  `nxp_isp` (i.MX8MP VVCAM) are registered today; `generic_usb` (UVC) and `rtsp_in`
  (proxy/re-encode an existing IP camera) are still empty stubs.
- Remaining: `VideoSource` trait extension — capabilities query, format negotiation, sensor
  controls (exposure/gain/WB/flip), hotplug/error recovery, hardware timestamp source.
- Same registry pattern for encoders (F1) and overlay renderers (F6) — not started.

### F12 — Configuration system
- Layered config: compiled defaults → `/etc/iiotedge/firmware.toml` → per-device
  overlay → env vars (`IIOTEDGE__…`, same convention as the SDK) → runtime commands.
- Strict validation with actionable errors at boot; `--check-config` CLI mode.
- Hot-reload without restart where safe (overlays, AI thresholds, log level);
  restart-required sections reported explicitly.
- Full schema documented in `docs/CONFIG.md`; versioned config with migration.

### F13 — Production hardening (cross-cutting)
- Observability: structured JSON logs, Prometheus `/metrics`, `/healthz`,
  per-stage pipeline latency + drop counters (fills the empty `core/metrics.rs`).
- Supervision: keep the thread-watchdog, add per-stage heartbeats, systemd watchdog,
  bounded restart/backoff (reuse `iiotedge_core::backoff` semantics).
- Security: no default passwords, per-service users, TLS everywhere, signed OTA,
  minimal open ports, secrets never logged.
- Quality: unit + integration tests (mock HAL makes the whole pipeline testable on CI),
  `cargo clippy -D warnings`, CI build for host + aarch64 cross target.

---

## 3. Architecture (target)

```
                                ┌────────────────────────────── tokio runtime ─┐
 machine PLCs ── Modbus/OPC UA ─┤ southbound drivers (iiotedge-protocols)      │
                                │        │                                     │
                                │   correlation engine ◄── clock sync (PTP)    │
                                │        │                                     │
 cam sensor ─► HAL v2 ─► FrameRouter ─┬─► AI engine (ort) ─► events ─► EngineHandle.ingest
 (real-time threads, CPU-pinned)      │        │                    (iiotedge-core engine,
                                      │     overlays                 SqliteBuffer persist-first,
                                      ├─► overlay+encoder (H264/H265)─ MqttTransport/SparkplugB)
                                      │        ├─► RTSP server / ONVIF │
                                      │        └─► segment recorder ──┘ clips/index
                                      └─► tamper analyzer ─► events
        ONVIF (WS-Discovery/SOAP) · SNMP agent · Prometheus · cluster bus (MQTT/mDNS)
```

Real-time video path stays on dedicated OS threads (as today); everything network-
facing runs on a tokio runtime because `iiotedge-lib` is async. A thin bridge
(bounded channels) connects the two worlds.

### F14 — LiDAR & multi-sensor fusion
- **LiDAR HAL** mirroring the camera HAL: a `PointSource` trait + registry with one
  backend module per device family — 2D safety/zone scanners (SICK TiM/LMS, Hokuyo,
  RPLidar), 3D LiDAR (Ouster, Velodyne, Livox, Robosense/Hesai) and point ToF
  rangefinders (serial/CAN — ride the iiotedge-lib drivers). Device swap is
  config-only: transport, rates, FOV/range crop, mounting pose.
- **LiDAR analytics**: zone presence/intrusion with debounced state machines
  (assistive warning/protective fields), min-distance streams, object clustering with
  size/velocity/trajectory, directional counting, silo/stockpile level and conveyor
  volumetric throughput, background learning + dust/rain filtering. Events and metrics
  flow through the same persist-first GDE pipeline; scan snapshots join the evidence index.
- **LiDAR + camera**: config extrinsics + guided calibration; depth projected onto the
  video (colorized overlay, distance labels burned into RTSP and recordings);
  cross-triggering both directions (zone violation → snapshot/clip; camera event →
  distance confirmation) with fused, timestamp-paired evidence.
- **LiDAR + camera + AI**: frustum association gives every AI detection a distance,
  real size, position and speed; multi-modal safety rules (PPE × proximity × speed —
  "no helmet within 2 m of the press"), near-miss statistics, volumetric anomaly
  detection, occupancy/dwell heatmaps; annotated overlays; cluster-mode fusion across
  nodes later (F9).
- **Presets**: shipped config templates for safety-zone guarding, silo level, gate
  counting, forklift safety.

### F15 — Local LLM bridge & agentic control (isolated, optional)
- A **strictly separated** `src/agent/` subsystem: compiled out via cargo feature or
  disabled via `[agent] enabled=false` (default). It interacts with the firmware only
  through the operator surfaces that already exist — command channel, telemetry/event
  stream, evidence index, config API — never through private hooks, so the video, AI,
  storage and safety paths are byte-identical with the agent on or off.
- Bridge to **local** LLM runtimes (Ollama / llama.cpp / vLLM on the LAN, OpenAI-compatible
  API; on-device NPU SLMs later behind the same trait). No cloud dependency.
- Agentic control with **deny-by-default capability allowlists** per tool (snapshot, clip,
  status, validated config changes, LiDAR zone queries, evidence search, reboot …),
  hard interlocks (can never disable tamper/recording/telemetry), rate limits, dry-run
  approval mode, and a full audit trail (every prompt + tool call → evidence index +
  GDE `agent_action`).
- Behaviors declared in config: event-driven follow-ups (tamper → snapshot burst + clip +
  written incident note), natural-language ops over the command channel grounded in the
  evidence index, incident report drafting, scheduled health patrols, and later
  site-level reasoning across the cluster bus (F9).

### F16 — Radar sensing & cross-modal cluster fusion
- **Radar HAL**: a `RadarSource` trait + registry mirroring the LiDAR `PointSource` and
  camera `VideoSource` patterns — config-only device swap. Two output tiers: point/detection
  radars (TI IWR6843/IWR1843, Acconeer A121, Xandar Kardian) and track-list radars that do
  their own onboard clustering (Continental ARS408 over CAN — rides the existing CAN/J1939
  southbound driver, Smartmicro, Navtech CIR/RAS for through-fog/dust perimeter security).
  Velocity (Doppler) and RCS come from the sensor for free — no tracker needed.
- **Why radar**: sees through dust, fog, smoke and darkness where camera and LiDAR both
  degrade — the standard sensing layer for quarry/mine haul roads and perimeter security,
  directly relevant to this firmware's iotmining cloud-relay integration. Radar analytics
  (zone presence, direct speed, directional counting, micro-Doppler human/vehicle/vegetation
  discrimination) work standalone, no camera required.
- **Radar + camera + AI + LiDAR fusion**: detections gain velocity for free wherever radar
  overlaps an AI box or LiDAR cluster; visibility-adaptive arbitration promotes radar to
  primary detector (camera/AI drop to verification-only) when a low-light/weather heuristic
  or schedule indicates degraded optical conditions; multi-modal safety rules (proximity ×
  speed × class) extend the AI rules engine (Phase 16) rather than defining a new schema.
- **Cluster-mode cross-device, cross-modal fusion** (the core ask this phase answers):
  generalizes `cluster::fusion::DetectionFusion` beyond same-label camera matching to
  modality + position/zone + time-tolerance matching. No wire-protocol break — the existing
  `MessageKind::Event{kind, summary, source_device}` already carries a free-form `kind`
  string; radar/LiDAR add new `kind` values on the same variant every peer already
  deserializes. Fusion stays peer-to-peer over the existing broker-less mesh — no central
  fusion server — which is what makes this an Industry 4.0-consistent architecture
  (interoperable event model, decentralized processing) rather than a hub-and-spoke design
  with a buzzword label.
- **Presets**: haul-road safety, all-weather perimeter, gate people-counting, radar-based
  forklift safety.

### F17 — Fused 3D spatial world-model & AR overlay
- **Three layers, two of which are firmware's job**: data (fused 3D object state) and
  pixel-overlay generation belong here; an interactive 3D viewer/dashboard is a
  mobile-app/web-dashboard concern built against the data this phase produces — same
  boundary already drawn for QR onboarding UI (12a) and AI-rule zone-drawing UI (16d).
- **Monocular estimate today, real depth later**: since radar (F16) and LiDAR (F14) are
  both still design-only, this starts with camera-AI-alone ground-plane distance estimation
  (bounding-box bottom-center + known mounting height/tilt/FOV) — approximate, flagged
  explicitly via a `depth_source: Estimated | Measured` field, not silently presented as
  precise. The shared `Object3D { position, velocity, class, confidence,
  contributing_sensors }` type is the same shape radar/LiDAR fusion later populates more
  accurately — no schema change when real depth sensors land.
- **AR-style 3D overlay**: 3D wireframe boxes projected back onto the 2D video (perspective
  projection from the same extrinsics as F14/F16's camera fusion), distance/velocity labels,
  optional ground-plane distance rings — extends `stream/overlay.rs`'s existing `DrawBox`
  pipeline rather than replacing it.
- **Cross-sensor fusion** (blocked on F14/F16 HAL landing): track association merges a
  camera detection + LiDAR cluster + radar track referring to the same real-world object
  into one `Object3D`; ties into F16's cluster-mode modality fusion so a fused object is one
  `object3d_event` on the mesh, not a separate wire format per sensor combination.
- **Data export**: GDE telemetry stream of `Object3D` snapshots is the actual "3D
  representation" contract — any 3D rendering (web dashboard, mobile AR view) is built
  against this by whichever repo owns that UI, explicitly not built here.
