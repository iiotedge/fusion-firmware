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
- [x] **Cloud-push relay** (2026-07-19): `stream_start`/`stream_stop` commands
      (src/commands.rs) drive an in-process GStreamer pipeline
      (src/stream/relay.rs: rtspsrc ! depay ! parse ! rtspclientsink) that
      republishes the local RTSP feed to media-ingestion-service's MediaMTX —
      the firmware half that service's own README was waiting on. Off by
      default ([cloud_relay].enabled); the LAN-only RTSP server is
      unaffected either way. Not yet exercised against a physical MediaMTX
      instance — verify `rtspclientsink` is present on-device
      (`gst-inspect-1.0 rtspclientsink`) and do a real push+playback round
      trip before relying on this in the field.

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
## workflow actions) — NOT STARTED, design-only entry 2026-07-24, requested by
## Santosh: "add a feature to customize object detection or any type of
## fencing... full flexibility in AI... like workflow"

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
- [ ] **Generalizes, does not replace,** `motion.zones` (`config.rs`
      `MotionZone` — normalized `[0,1]` **rectangles only**, luma-diff
      based, stays exactly as-is: cheaper, non-AI, different trigger). New
      `[[ai.rules]]` array-of-tables, each rule: `name`, `enabled`,
      `classes` (subset of `ai.labels`, empty = any), `min_confidence`
      (optional override of `ai.confidence_threshold`), `zone` (list of
      normalized `[x,y]` points — 2 points = line, 3+ = polygon, so line-
      crossing and intrusion share one geometry field instead of two config
      shapes), `mode` (`"presence"` | `"line_cross"` | `"loiter"`),
      `direction` (line_cross only), `dwell_s` (loiter only)
- [ ] Per-rule `schedule` — reuse `ScheduleConfig`'s exact day/time-window
      pattern (`schedule.rs`) instead of a new scheduling format, so a rule
      can be "person in zone A, but only 10pm–6am"
- [ ] Config validation at boot (mirrors the existing `ai.roi` length check
      in `config.rs`): reject self-intersecting/degenerate polygons,
      out-of-range coordinates, unknown class names — actionable error,
      not a silent no-op rule

### 16b — Rule evaluation engine
- [ ] New `src/ai/rules.rs` — post-processing layer between the parser
      (`ai/parser.rs`, untouched — still just decodes raw model output) and
      wherever detections currently get published, mirroring the existing
      separation where `tamper.rs`/`motion.rs` are independent analyzers,
      not changes to the shared pipeline
- [ ] Point-in-polygon (ray casting) + point-on-line-segment-with-direction
      tests against each detection's bottom-center point (see convention
      note above)
- [ ] Loiter tracking needs object persistence across frames (a rule fires
      once dwell_s is exceeded, not once per frame) — the AI engine has no
      tracker today (parser.rs is single-frame NMS only); this is the
      one genuinely new piece of state, not just config plumbing. Simplest
      viable approach: per-rule, per-zone "seen since" timestamp keyed by
      (class, coarse position bucket) — full multi-object tracking (SORT/
      ByteTrack-style ID assignment) is a larger, separate lift, only take
      it on if the simple approach proves too false-positive-prone
- [ ] A rule match produces a `rule_event` (name, mode, class, confidence,
      zone) — feeds the SAME downstream paths genuine detections already
      use: GDE telemetry event, cluster `ai_event` broadcast (so cross-
      device fusion also sees rule-gated events, not just raw detections),
      snapshot/clip triggers

### 16c — Actions (what a rule *does*, the "workflow" part of the ask)
- [ ] Per-rule `actions = [...]`: `telemetry_event` (always implicit),
      `snapshot`, `clip` (reuse `ClipExtractor`, same pre/post-roll as
      today), `cluster_broadcast` (reuse `cluster/fusion.rs`'s existing
      broadcast path)
- [ ] `webhook`: HTTP POST to a configured URL with the `rule_event` JSON —
      the standard "wire this into anything" integration hook every
      surveyed product has in some form (Frigate: MQTT+webhooks; Axis/
      Hikvision/Dahua: HTTP notification profiles) and this firmware
      doesn't have any equivalent of yet
- [ ] `gpio_output`: pulse a configured GPIO line (siren/relay/light) —
      `gpio-cdev` is already a dependency (`hal/`'s button/GPIO support),
      this reuses it rather than adding a new one
- [ ] Explicitly OUT of scope for this phase: PTZ preset actions (no PTZ
      support exists anywhere in this firmware yet — would need its own
      phase first)

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

## Phase 17 — Radar sensing & cross-modal fusion (F16) — DESIGN-ONLY,
## NOT STARTED, requested 2026-08-02: "integrate radar sensors, work in
## cluster mode along with AI and other available features [since] we are
## connecting together — plan accordingly as per Industry 4.0"

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
- [ ] `RadarSource` trait + registry, mirroring `PointSource` (14a) and the
      camera `VideoSource` registry (`src/hal/mod.rs`) exactly — same
      `register_source()`-style pattern, not a third bespoke factory shape
- [ ] Two output tiers, because industrial radar hardware genuinely splits
      this way — model both, don't force one into the other:
      - **Point/detection-list radars** (short/mid range, cheap): TI
        IWR6843/IWR1843 mmWave (UART, binary TLV frame protocol — TI's
        mmWave Industrial Toolbox), Acconeer A121/XM125 (I²C/SPI/UART,
        ultra-low-power presence + short-range distance — doorway/gate
        tier), Xandar Kardian XK series (UART/USB, presence + people
        counting)
      - **Track-list radars** (long range, automotive/industrial grade,
        already do internal clustering+tracking): Continental ARS408
        (**CAN bus** — rides the existing `Serial, CAN/J1939` southbound
        driver in iiotedge-lib almost for free), Smartmicro DRVEGRD/UMRR
        (UDP or CAN), Navtech CIR/RAS (TCP — purpose-built for
        through-fog/dust perimeter security, the mining-industry-standard
        radar for exactly this firmware's target sites)
- [ ] `RadarFrame` type: either `points[range, azimuth, elevation,
      velocity, rcs]` (point tier) or `tracks[id, range, azimuth,
      velocity, rcs, class_hint]` (track tier) — velocity and RCS
      (radar cross-section, a coarse size/reflectivity proxy) are the two
      fields LiDAR's `PointCloud` doesn't have and radar gets for free
- [ ] Config: `[radar]` type, transport (CAN id | UDP/TCP host:port |
      serial device+baud), FOV/range crop, mounting pose (x,y,z,roll,
      pitch,yaw) — same extrinsic reference frame convention as `[lidar]`
- [ ] Health: interference/blockage/saturation detection (radar-specific
      failure modes — mutual interference from nearby radars, obstruction
      by mud/ice on the radome) → tamper-style events, same pattern as
      14a's packet-loss/rotation-stall/dirty-window checks

### 17b — Radar analytics (standalone, no camera needed)
- [ ] Zone presence/intrusion — same sustain+recover state machine as
      tamper.rs/14b, reused a third time, not reinvented
- [ ] Direct speed measurement (no estimation needed — Doppler gives
      velocity per point/track): haul-road vehicle speed, conveyor speed,
      person walking speed, all without a camera in the loop
- [ ] Directional counting (people/vehicle) on gates and lanes, same as
      14b's LiDAR counting but usable in zero-visibility conditions
- [ ] Micro-Doppler classification (vibration/gait signature): coarse
      human-vs-vehicle-vs-vegetation discrimination — a capability neither
      camera AI nor LiDAR has, genuinely radar-specific, worth flagging as
      a distinct value-add rather than "AI but worse resolution"
- [ ] Background/clutter learning (static reflectors, foliage) — radar's
      equivalent of 14b's dust/rain filtering

### 17c — Radar + camera fusion
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

### 17d — Radar + camera + AI + LiDAR fusion
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
- [ ] Generalize `cluster::fusion::DetectionFusion` (`src/cluster/fusion.rs`)
      beyond same-label string matching: today it queues `(label,
      Instant)` pairs and matches a peer's `ai_event` summary against
      them — radar/LiDAR events don't have a class label the same way, so
      matching needs to extend to modality + approximate position/zone +
      time tolerance, not label text alone. Camera-only fusion keeps
      working exactly as today; this is additive, not a rewrite
- [ ] **No wire protocol change needed for the transport itself** —
      `MessageKind::Event { kind, summary, source_device }`
      (`src/cluster/mod.rs`) already carries a free-form `kind` string
      (today: `"tamper"`, `"motion"`, `"ai_event"`, …); radar/LiDAR add new
      `kind` values (`"radar_track"`, `"radar_zone"`, `"lidar_zone"`) on
      the same enum variant every peer already deserializes — a genuinely
      additive change, not a breaking one, so mixed-version mesh nodes
      stay compatible during rollout
- [ ] Cross-device corroboration: the same physical object (vehicle,
      person) crossing overlapping radar coverage between two mesh nodes
      — or a radar detection on node A corroborating an AI detection on
      node B's camera — is one real-world event, not two, exactly the
      existing camera-fusion principle (`fusion.rs`'s doc comment)
      generalized across sensor types instead of just across cameras
- [ ] Site-level fused view: `/cluster/status` (already exists) gains a
      per-node sensor inventory (camera/radar/LiDAR present + healthy) so
      an operator — or the Phase 15 LLM bridge, later — can reason about
      site-wide coverage, not just per-device state
- [ ] Decentralization stays a hard requirement, not just a Phase 9
      carryover: fusion runs peer-to-peer over the existing broker-less
      mesh transport (WiFi multicast / BLE), no central fusion server —
      this is what makes it an Industry 4.0-consistent architecture
      (interoperable event model, decentralized processing) rather than a
      buzzword label on a conventional hub-and-spoke design

### 17f — Use-case presets (config templates shipped in `config/presets/`)
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
