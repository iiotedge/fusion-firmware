# Fusion Firmware — Production Feature Specification

Target: production-ready, mass-deployable edge vision firmware for Industry 4.0 — not
just a camera driver: on-device AI, cross-device cluster mesh, cloud relay, QR
onboarding, and telemetry with southbound machine-data ingestion, all in one binary.
SDK policy: **consume `iiotedge-lib` as-is** (telemetry engine, store-and-forward buffer,
TLS/TPM security, southbound machine drivers). The lib is never modified from this repo.

---

## 1. Current state (baseline, v1.0.0)

What exists and is kept (nothing is removed, only refactored/extended):

| Area | File(s) | Status |
|---|---|---|
| Multi-thread pipeline (capture → AI / encoder) w/ CPU pinning | `src/main.rs` | Working skeleton |
| Zero-copy frame router (bounded, drop-on-full) | `src/core/ring_buffer.rs` | Working |
| DMA buffer abstraction | `src/core/memory.rs` | Working (unused re-queue hook) |
| HAL trait + factory (`VideoSource`) | `src/hal/mod.rs` | Working |
| Mock camera (macOS dev) | `src/hal/mock_cam.rs` | Working |
| Generic V4L2 camera | `src/hal/generic_v4l2.rs` | **Broken** (`FrameHandle::dummy()` doesn't exist, bad import) — never compiled (Linux-gated) |
| NXP i.MX8MP ISP camera | `src/hal/nxp_isp.rs` | **Broken** (missing imports, incomplete trait impl) |
| USB camera | `src/hal/generic_usb.rs` | Empty stub |
| Hardware encoder | `src/stream/encoder.rs` | Stub (pipeline string only, GStreamer commented out) |
| RTSP server | `src/stream/rtsp_server.rs` | Empty stub |
| MQTT / commands | `src/network/*.rs` | Empty stubs |
| Metrics | `src/core/metrics.rs` | Empty stub |
| AI engine (ONNX) | `src/ai/engine.rs` | Simulated (ort commented out) |
| TOML config | `src/config.rs`, `config/iiotedge_default.toml` | **Broken**: duplicate `[camera]` table → parse fails at boot |
| Cross build | `Dockerfile.cross`, `build_target.sh`, `Makefile` | Present |

`iiotedge-lib` is currently **not referenced at all** — integration is new work.

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

### F2 — ONVIF conformance (Profile S baseline, Profile T stretch)
- WS-Discovery responder (device discoverable by VMS/NVRs).
- SOAP services: Device Management (info, capabilities, time, reboot, users),
  Media (profiles, stream URIs), Events (tamper/AI events via WS-BaseNotification pull point).
- Profile T items behind a feature flag: H.265 media2 service, metadata streaming.
- ONVIF user auth (WS-UsernameToken) backed by the same user store as RTSP.

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

### F6 — Industry 4.0 overlays (OSD), customizable
- Overlay engine compositing before encode, fully config-driven layout:
  - timestamp (clock-synced, ms precision), device id / facility / line id,
  - free text, image/logo (PNG), privacy masks,
  - AI bounding boxes + labels, tamper banner,
  - **live machine-data fields** (e.g. `line1/temp = 21.5 °C`) from southbound drivers (F7),
  - diagnostics (fps, bitrate, sync offset) toggleable.
- Per-stream-profile overlay sets; positions in relative coordinates; TTF font config.
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
- Device identity: stable `device_id` derived from hardware (MAC/SoC serial/TPM EK),
  overridable at provisioning; identity file with facility/line/position labels.
- Device footprint endpoint + MQTT birth payload: model, hw revision, SoC, sensor,
  firmware version + git hash, config hash, enabled features, uptime, storage health.
- **SNMP agent** (v2c read-only + v3 auth/priv): standard MIB-II system group plus a
  private IIoTEdge MIB (stream state, fps, tamper state, storage %, sync offset);
  SNMP traps for critical events.
- Provisioning: first-boot flow (per-device config overlay from USB/DHCP option/
  provisioning service), certificate enrollment, unique credentials — no shared secrets
  in the golden image.
- OTA: A/B firmware update hooks (RAUC/swupdate-compatible layout), signed artifacts,
  rollback on boot failure; model files updatable independently of firmware.
- Packaging: cross-compile targets (aarch64 — Radxa Zero 3E/RK3566 reference,
  i.MX8MP supported), systemd unit with watchdog (`sd_notify`), Yocto/Debian
  package recipe, single golden image + per-device overlay.

### F11 — HAL v2 (any camera, flexible & extendable)
- `VideoSource` trait extended: capabilities query, format negotiation, sensor controls
  (exposure/gain/WB/flip), hotplug/error recovery, hardware timestamp source.
- Registry-based factory (`register_source("VENDOR_X", ctor)`) instead of hardcoded
  match — new cameras are added by implementing one trait, no core changes.
- Backends: `mock` (dev), `generic_v4l2` (mmap + DMABUF), `generic_usb` (UVC),
  `nxp_isp` (i.MX8MP VVCAM), `rtsp_in` (proxy/re-encode an existing IP camera).
- Same registry pattern for encoders (F1) and overlay renderers (F6).

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
