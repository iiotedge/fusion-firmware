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
- [ ] Registry-based camera factory (replaces hardcoded match; any camera pluggable)
- [ ] Real `generic_v4l2` backend (mmap + DMABUF, control application from config)
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
- [ ] **PTZ support** (gap analysis 2026-07-17): ONVIF PTZ service + pluggable drive
      backends (Pelco-D over serial/RS-485, ONVIF PTZ passthrough for rtsp_in proxies);
      presets, patrol routes, PTZ-on-event (zone violation → preset)
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
- [ ] Device identity: hw-derived `device_id` (MAC/SoC serial/TPM), provisioning overlay, identity file
- [ ] Device footprint: birth payload + HTTP endpoint (model, hw rev, fw version+git hash, config hash, features)
- [ ] SNMP agent: v2c/v3, MIB-II system group + private MIB (streams, tamper, storage, sync), traps
- [ ] First-boot provisioning flow + certificate enrollment (no shared secrets in golden image)
- [ ] OTA A/B hooks (RAUC/swupdate layout), signed artifacts, rollback
- [ ] systemd unit with `sd_notify` watchdog; Yocto/Debian packaging; golden image + per-device overlay

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
      app-repo UI concern** — same boundary already drawn for Phase 11b/12a
      (see [[cluster-mesh-feature-progress]] memory) — not started, not
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
