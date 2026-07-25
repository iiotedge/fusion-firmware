# Fusion Firmware v1.0.0

Initial public release. Fusion Firmware is a config-driven edge vision
platform for Industry 4.0 deployments — video (RTSP/ONVIF), on-device AI
detection, cross-device cluster coordination, and telemetry with southbound
machine-data ingestion, all in one Rust binary, all driven by TOML.

## Video & streaming

- RTSP streaming via `gst-rtsp-server`, H.264/H.265 switchable per stream profile.
- Hardware encoder auto-probe: Rockchip MPP → NXP VPU → V4L2 stateful →
  VideoToolbox (macOS dev) → software x264, in that order, so the same config
  works across supported hardware without an explicit encoder choice.
- ONVIF Profile S: WS-Discovery responder + Device/Media SOAP services
  (`GetDeviceInformation`, `GetStreamUri`, `GetProfiles`, …), with optional
  WS-Security (UsernameToken, PasswordText/PasswordDigest) enforcement.
- NVR-style local recording: fixed-length MP4 chunks with independent
  size/age rotation, running its own encoder session independent of RTSP
  clients.
- On-video overlays: timestamp/device-id burn-in plus live detection boxes
  (label + confidence) with automatic staleness expiry.
- Evidence export: SD/USB mirroring and FTPS upload, automatic or
  command-triggered, with rotation caps and free-space warnings.
- On-demand cloud-push relay: a `stream_start`/`stream_stop` command pair
  pulls the local RTSP feed and republishes it (unmodified, no re-encode) to
  an external RTSP ingest endpoint — the LAN-only local RTSP server is
  completely unaffected either way.

## On-device AI & analytics

- AI inference via RKNN (Rockchip NPU) or ONNX Runtime, selected per device
  by config — the same YOLOv8 parser and letterbox preprocessing feed both
  backends, with backend-specific quirks (NHWC layout, input scale,
  normalized box coordinates) isolated to each backend module.
- Configurable class filter and confidence threshold; inference runs at a
  config-capped fps independent of capture/streaming fps.
- Tamper detection: blackout, blinding, occlusion, freeze, and scene-change,
  each independently configurable.
- Zone-based motion detection: cheaper than AI, the standard middle ground
  between "always record" and full inference.
- Machine-data correlation: pairs southbound events (serial/CAN/Modbus) with
  the exact frame on screen when they arrived, publishing a correlated event
  with both timestamps and their delta.

## Cluster mesh (multi-camera coordination)

- Broker-less: WiFi UDP multicast and/or Bluetooth LE advertising, so
  coordination keeps working through a broker/WAN outage — no MQTT, no cloud
  dependency for the mesh itself.
- Peer discovery (`Announce`) and active liveness probing (`Ping`/`Pong`),
  jittered to avoid same-config devices drifting into lockstep.
- Deterministic leader election (highest priority, tie-broken by node id) —
  self-healing, no election round-trip.
- Cross-device detection fusion: correlates this device's own recent AI
  detections against peers' broadcasts, producing one corroborated event
  when the same object crosses multiple camera fields of view.
- Peer-triggered re-analysis and agentic remote-command reactions
  (`[[cluster.reactions]]`) — a matched peer event can tell another peer to
  run a command, through the exact same audited path MQTT commands use.
- `GET /cluster/status` — read-only HTTP export (leader, peers, recent
  command acks) for the mobile app or any external consumer, no
  cluster-protocol client required.

## QR device onboarding

- `GET /onboarding/qr.png` and `/onboarding/info` serve a scannable QR code
  (and its JSON equivalent) carrying everything the mobile app needs to pair
  a camera in one scan: device identity, LAN address, ONVIF/RTSP/metrics
  ports, RTSP+ONVIF credentials, MQTT broker identity, and a bearer API
  token — no manual IP/port/credential entry.
- Two-tier auth: an installer-only token gates who can view the QR/JSON;
  a separate, narrower API token (embedded in the payload) is what the
  mobile app presents to `/cluster/status` — a compromised phone can never
  reach the command channel.
- Full mobile integration contract in [docs/QR_ONBOARDING.md](docs/QR_ONBOARDING.md).

## Telemetry & command channel

- Persist-first pipeline via the [iiotedge-lib] SDK: every event lands in a
  SQLite write-ahead-log buffer *before* any network attempt — broker
  outages and power loss never lose accepted data (FIFO, at-least-once).
- Northbound MQTT in GDE JSON envelope or Sparkplug B, with TLS/mTLS/TPM
  support.
- Southbound machine drivers — serial (RS-232/485), CAN/J1939, Modbus TCP —
  added by config alone, no code changes.
- MQTT command channel: `status`, `snapshot`, `clip`, `config_get`,
  `reboot`, `export`, `stream_start`/`stream_stop`, `reanalyze`, all
  audit-logged and bearer-token authenticated.

## Reliability

- Thread-liveness watchdog: every worker (capture, analytics, encoder,
  cluster) beats a heartbeat; a wedged thread trips a clean supervised exit
  for the init system to restart, rather than hanging silently.
- Config schema uses serde defaults throughout — a config file written for
  an older firmware version keeps parsing rather than bricking a fleet.

## Known limitations

- Customizable AI detection rules (zones, line-crossing, loitering, custom
  workflow actions) are designed but not yet implemented — see
  [TODO.md](TODO.md) Phase 16.
- SNMP agent and broader fleet-management footprint are not yet implemented.
- The [iiotedge-lib] SDK dependency is consumed via a path reference to a
  sibling checkout and is not included in this repository.

Full as-built detail, phase by phase: [docs/FEATURES.md](docs/FEATURES.md) and
[TODO.md](TODO.md).
