# Fusion Firmware

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Production-grade **edge vision platform** in Rust — not just a camera driver.
One config-driven binary covers hardware-accelerated RTSP/ONVIF, on-device AI
detection, tamper and motion analytics, cross-device cluster coordination,
QR-code device onboarding, cloud relay, and a persist-first telemetry
pipeline with southbound machine-data ingestion (serial/CAN/Modbus). Deployable
at fleet scale from one TOML file per device.

**Reference device:** Radxa Zero 3E (Rockchip RK3566) with a CSI camera and
Rockchip NPU (RKNN) for on-device AI. NXP i.MX 8M Plus and generic UVC/V4L2
hardware are supported through the same config-driven HAL. Dev machines
(macOS) run a mock camera with a synthetic test pattern — every subsystem,
including AI inference hooks, works without hardware.

```
 CSI/USB camera ─► HAL (GST_V4L2 / raw V4L2 / mock) ─► FrameRouter (bounded, drop-on-full)
                                                        ├─► Analytics thread: AI (RKNN/ONNX)
                                                        │     + tamper + motion + correlation
                                                        │     + AI detection rules (zones/lines/loiter)
                                                        │     ──► events + fused detections ─┐
                                                        ├─► RTSP server (mpph264enc/…) ──┐    │
                                                        │     └─► optional cloud relay   │    │
                                                        └─► NVR chunk recorder (MP4+rotation) │
     mic (optional) ─► audio fan-out ─► RTSP audio track + NVR audio track            │    │
   machine data (serial / CAN / Modbus …) ─► iiotedge-lib southbound ─┬─► video widgets (sparkline/gauge)
                                                                      ▼                  ▼    ▼
                                            SQLite store-and-forward buffer ─► MQTT (GDE JSON / Sparkplug B)

   Cluster mesh (WiFi multicast / BLE) — peer discovery, leader election, cross-device
   detection fusion, agentic remote-command reactions, /cluster/status for the mobile app

        ONVIF WS-Discovery + SOAP · QR device onboarding · systemd watchdog · fleet Makefile
```

## Features

| Area | Status | Notes |
|---|---|---|
| RTSP streaming | ✅ | `gst-rtsp-server`, shared pipeline, H.264/H.265 switchable |
| Hardware encoding | ✅ | Auto-probed: Rockchip MPP → NXP VPU → V4L2 stateful → VideoToolbox → software |
| Audio | ✅ | Off by default; one capture feeds both RTSP (RTP track) and NVR chunks (audio track), opus/aac |
| ONVIF | ✅ | WS-Discovery + Device/Media SOAP (GetStreamUri, Profiles, …), WS-Security auth |
| PTZ control | ✅ | Off by default; ONVIF PTZ service + MQTT commands share one controller — Pelco-D (RS-485/RS-232) backend today, ONVIF passthrough planned |
| NVR local recording | ✅ | Fixed-length MP4 chunks, size/age rotation, independent encoder session |
| SD/USB + FTPS evidence export | ✅ | Mirrors chunks/clips/snapshots off-device, auto or on-demand |
| On-device AI detection | ✅ | RKNN (Rockchip NPU) or ONNX Runtime backend, YOLOv8 parser, configurable class filter |
| Tamper detection | ✅ | Blackout, blinding, occlusion, freeze, scene-change |
| Zone motion detection | ✅ | Configurable zones, cheaper middle ground between "always record" and AI |
| Machine-data correlation | ✅ | Pairs southbound events (serial/CAN/Modbus) with the frame on screen when they arrived |
| On-video overlays | ✅ | Timestamp/device-id burn-in + live detection boxes with label/confidence |
| Live machine-data widgets | ✅ | Sparkline trends, bar gauges, big-number values burned into stream + recordings from any southbound tag, dependency-free renderer |
| Cluster mesh | ✅ | Broker-less WiFi multicast / BLE — peer discovery, leader election, cross-device detection fusion, peer-triggered re-analysis, agentic remote-command reactions, `/cluster/status` |
| QR device onboarding | ✅ | Scan-to-pair for the mobile app: connection info, credentials, and API token in one QR code |
| Cloud-push relay | ✅ | On-demand relay to a cloud media server, LAN-only local stream untouched; RTSP or WebRTC (WHIP) transport per stream_start call, auto-reconnect with backoff |
| Telemetry (persist-first) | ✅ | [iiotedge-lib] engine: SQLite WAL buffer → MQTT GDE JSON / Sparkplug B |
| Machine data southbound | ✅ | Serial, CAN/J1939, Modbus TCP — **config-only** in `config/edge.toml` |
| Thread-liveness watchdog | ✅ | Per-worker heartbeats; a wedged thread trips a clean supervised restart |
| Device identity + footprint | ✅ | Hardware-derived `device_id`, `GET /footprint` (model, fw version+git hash, config hash, features), GDE birth event |
| SNMP agent | ✅ | v2c, MIB-II + private enterprise MIB, traps — off by default, verified against real `snmpget`/`snmpwalk` |
| systemd watchdog + `.deb` packaging | ✅ | `sd_notify` tied to the firmware's own liveness check; `make deb` (apt/local-repo fleets) alongside the existing tarball |
| Customizable AI detection rules (zones/line-crossing/loitering/workflow actions) | ✅ | Presence/line-crossing/loiter modes, per-rule schedule; actions: snapshot/clip/cluster broadcast/webhook/GPIO output |
| Home Assistant integration | ✅ | MQTT Discovery (binary_sensors + Snapshot/Clip buttons); RTSP/ONVIF already work with HA's built-in camera integrations, no firmware change needed |
| Zigbee / Z-Wave southbound bridge | ✅ | Subscribes to Zigbee2MQTT / Z-Wave JS UI (or any JSON-over-MQTT source), feeds the same correlation engine as industrial southbound tags |
| Remote AI/automation config | ✅ | Read/write `[[ai.rules]]` over MQTT (`config_get_ai_rules`/`config_set_ai_rules`) or `GET`/`POST /config/ai-rules`; survives a restart, applies live with no reboot |
| Radar sensing (HAL + analytics + cluster fusion) | ✅ | `RadarSource` trait + registry, zone/line-cross/loiter analytics, cross-device zone fusion over the mesh — proven against a tested `mock` backend; real vendor hardware (TI mmWave, Continental ARS408, Navtech, ...) not built, see [TODO.md](TODO.md) Phase 17a |
| First-boot cert enrollment / OTA A/B updates | 🔜 | Design-only, blocked on a provisioning server and an OS image A/B layout that don't exist yet — see [TODO.md](TODO.md) Phase 12c/12d |
| Matter (smart-home) support | ✅ | Off by default; genuinely generic, not camera-only — `[matter.camera]` (Matter 1.5 Camera: WebRTC Transport, Camera AV Stream, Zone Management), `[matter.onoff]` (a plain On/Off Light/Switch backed by a real GPIO line), `[matter.light]` (a full Extended Color Light: dimming + hue/saturation/XY/color temperature/color loop), and `[matter.thermostat]` (Thermostat, 0x0201) are independent, config-selected endpoints on the same firmware binary. Camera: real SDP/ICE via `str0m`, live H.264 tap (real hardware encoder on the Radxa target), `[[ai.rules]]` zones reflected read-only, scan-to-add via `GET /onboarding/matter-qr.png`; deployed to real hardware and commissioned into a real Apple Home fabric (commissioning + Basic Information all correct; no camera view in Apple Home yet — their app has no Matter Camera cluster support today, a controller-side gap). OnOff: `[matter.camera].enabled=false` + `[matter.onoff].enabled=true` deploys this exact firmware as a plain Matter light switch, no camera clusters at all. Light: checked the installed `rs-matter 0.3.0` source directly rather than assume from the spec — its ready-made *application handlers* (hooks + spec-rule enforcement) cover only Lighting (On/Off/Level/Color) and the camera clusters above — but it also ships typed, spec-generated declarations for essentially every Matter cluster (thermostat, fan, door lock, window covering, every sensor, energy, closures, media, …), so the other device types are buildable by implementing those typed traits (see [TODO.md](TODO.md) Phase 19g for the generic plan). No PWM/RGB driver exists on this board yet, so the light's state is honestly in-memory only — real, spec-compliant, and controller-verified, just not driving physical hardware. Thermostat is currently built directly against rs-matter's lower-level `Handler` trait (slated to move onto its typed, spec-generated cluster layer — TODO.md Phase 19g) — `LocalTemperature` is real (SoC thermal-zone reading), `SystemMode`/setpoints are honestly virtual (no HVAC equipment on this board); carries an explicit lower confidence note until it gets a real-controller pairing pass. Beyond those four, `[[matter.endpoints]]` makes it generic: any number of temperature / humidity / pressure / flow / light-level / occupancy (incl. Matter 1.5 radar/vision technologies) / contact / water-leak / rain / freeze / soil-moisture (Matter 1.5) sensors (with Matter change events), each bound to a data source by a spec string (a built-in firmware signal, any Linux sysfs file such as IIO/hwmon/1-Wire, a GPIO input, or a value pushed over `POST /signals/<name>`) — no reading means Matter `null`, never a made-up value — plus any number of lights, plugs and fans, each bound to an output (a GPIO relay line, a named signal other software acts on, or virtual), and buttons/switches that emit Matter press events (short / long / double press, latching toggles), so one device can be a light AND a fan AND a sensor AND a button. A fan only offers the speeds it really has, and a failed hardware write is never reported as success. Dispatch is a flat registry/router (no per-cluster type nesting), and everything is verified end-to-end against an independent Matter controller (`make matter-verify`, matter.js, 176 checks). Test-only device attestation (no CSA cert), no PTZ-over-Matter — see [TODO.md](TODO.md) Phase 19c-19f |

## Quick start (dev machine, no hardware)

```bash
make run           # mock camera → RTSP on rtsp://127.0.0.1:8554/live
ffplay rtsp://127.0.0.1:8554/live
curl -s -X POST -H 'Content-Type: application/soap+xml' \
  --data '<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><tds:GetDeviceInformation xmlns:tds="http://www.onvif.org/ver10/device/wsdl"/></s:Body></s:Envelope>' \
  http://127.0.0.1:8000/onvif/device_service
```

Requires GStreamer dev libraries (`brew install gstreamer` / `apt install
libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstrtspserver-1.0-dev`)
and the sibling checkout of [iiotedge-lib] at `../iiotedge-lib` (a separate,
private SDK repository — see [Dependencies](#dependencies) below).

## Deploy to a device (Radxa Zero 3E)

```bash
make docker-image                      # one-time: cross toolchain container
make deploy DEVICE_IP=192.168.1.13     # cross-compile release + push binary (+ service restart)
make deploy-config DEVICE_IP=...       # push config/ (edge.toml + firmware config)
make install-service DEVICE_IP=...     # systemd unit: boot-persistent, auto-restart
make device-logs DEVICE_IP=...         # live journal tail
```

Device prerequisites (Radxa OS): camera overlay enabled via `sudo rsetup`
(e.g. *Raspberry Pi Camera v1.3 (OV5647)*), GStreamer plugins good/bad and
`gstreamer1.0-rockchip-mpp` for hardware encoding, `librknnrt.so` for on-device
AI. If capture fails at boot, the log prints a scan of every `/dev/video*`
node with driver + capabilities.

**Before shipping a real device:** `config/iiotedge_default.toml` ships with
`CHANGE-ME` placeholders for `[security].command_token`, `[security].api_token`,
and the default user's password — generate a unique random value per device
(`openssl rand -hex 24`) rather than reusing the same secret across a fleet.

## Releases

Every tag (`vX.Y.Z`) triggers a full pipeline run (fmt, clippy, test, security
audit, cross-compile check) followed by a build for each supported target,
published to [GitHub Releases](https://github.com/iiotedge/fusion-firmware/releases)
as a self-contained tarball (binary + default config + systemd unit + sha256):

| Target | Devices |
|---|---|
| `aarch64-unknown-linux-gnu` | Radxa Zero 3E (RK3566), i.MX 8M Plus |
| `x86_64-unknown-linux-gnu` | Generic Linux with a V4L2/USB camera, or `camera.type = "MOCK"` for a hardware-free node |

```bash
tar -xzf fusion-firmware-*.tar.gz
cd fusion-firmware-*/
sha256sum -c ../fusion-firmware-*.tar.gz.sha256   # verify before deploying
```

Building from source (`make deploy`, below) stays the primary path for
active development; releases are for reproducible, versioned fleet rollouts.

## QR device onboarding

Pairing a camera with the mobile app doesn't require typing in an IP address,
ports, or credentials by hand. The firmware serves a QR code
(`GET /onboarding/qr.png`, alongside `/metrics` on the same port) encoding
everything the app needs — device identity, LAN address, ONVIF/RTSP/metrics
ports, RTSP+ONVIF credentials, and a bearer token for `/cluster/status` — in
one scan. Full contract for building a scanner against it:
[docs/QR_ONBOARDING.md](docs/QR_ONBOARDING.md).

## Configuration

Everything is driven by two files (see inline comments for every option):

- **[config/iiotedge_default.toml](config/iiotedge_default.toml)** — the firmware:
  camera HAL (`GST_V4L2` | `SYSTEM_GENERIC` | `NXP_ISP` | `MOCK`), resolution/format,
  stream codec `h264|h265` + encoder auto-probe candidates, RTSP port/path,
  ONVIF identity, AI runtime/model/class-filter, tamper/motion/correlation
  rules, cluster mesh, QR onboarding, NVR storage (chunk length, rotation
  caps), logging.
- **[config/edge.toml](config/edge.toml)** — the [iiotedge-lib] SDK: store-and-forward
  buffer, MQTT northbound (GDE JSON envelope or Sparkplug B, TLS/mTLS/TPM), and
  southbound machine drivers. Adding a serial scanner or CAN bus is **config-only**:

```toml
[[southbound.serial]]
name = "scanner1"
port = "/dev/ttyUSB0"
baud_rate = 115200

[[southbound.canbus]]
name = "vehicle"
interface = "can0"
```

### Use-case presets

[config/presets/](config/presets/) ships 21 complete, deployable configs:
10 single-camera Industry 4.0 scenarios (perimeter intrusion, restricted
machine safety zones, dock loitering, gate counting, production-line
correlation/widgets HMI, multi-camera cluster mesh, after-hours lockdown,
PPE compliance, cold-storage tamper monitoring, forklift/pedestrian shared
lanes), 5 `cluster-fusion-*` multi-camera deployments built around
cross-device detection fusion for target-customer verticals beyond the
factory floor (retail loss prevention, critical infrastructure, campus
security, construction sites, smart parking), 5 `home-*` residential
smart-home scenarios built around Home Assistant/Zigbee integration (front
door, driveway arrival→HA-lighting, garage, pool safety, whole-house
mesh), and `radar-mock-demo.toml` (Phase 17) — an end-to-end radar-sensing
pipeline demo on the `mock` backend. Each is a drop-in replacement for
`config/iiotedge_default.toml`
(`cp config/presets/<name>.toml config/iiotedge_default.toml`), built
entirely on what's shipped today (AI detection rules, PTZ, SNMP, cluster
mesh + fusion, correlation, overlays/widgets, audio, Home Assistant/
Zigbee/Z-Wave) and checked by
`config::tests::every_shipped_preset_parses_and_validates` in CI so they
can't silently bit-rot. See [config/presets/README.md](config/presets/README.md)
for the full list and what each one showcases.

Machine data and camera events (AI detections with capture timestamps, tamper,
motion, correlated hits, health) travel the same pipeline: persisted to
SQLite **before** any network attempt — broker outages and power loss never
lose accepted data (FIFO, at-least-once).

## Repository layout

```
src/
  main.rs            boot + thread supervision (capture / analytics / media / watchdog)
  config.rs          firmware config schema (serde defaults = no fleet bricking)
  hal/               camera backends: gst_v4l2 (MPLANE ISPs), generic_v4l2 (UVC),
                     nxp_isp, mock_cam + /dev/video* diagnostics
  ai/                inference engine — RKNN (Rockchip NPU) / ONNX Runtime backends,
                     YOLOv8 parser, letterbox preprocessing, customizable detection
                     rules (zones/line-crossing/loitering) + workflow actions
  audio.rs           microphone capture, fans out to RTSP audio track + NVR audio track
  tamper.rs          blackout/blinding/occlusion/freeze/scene-change detection
  motion.rs          zone-based motion detection
  correlation.rs     pairs southbound machine events with the frame on screen
  mqtt_bridge.rs     Zigbee2MQTT/Z-Wave JS UI (or any JSON-over-MQTT source) → correlation.rs
  homeassistant.rs   Home Assistant MQTT Discovery: binary_sensors + Snapshot/Clip buttons
  runtime_config.rs  remote ai.rules read/write (MQTT + HTTP), restart-persistence, live apply
  radar/             radar HAL (RadarSource trait + registry, mock backend) + zone/
                     line-cross/loiter analytics — real vendor backends not built
  matter/            Generic Matter device (off by default): commissioning + mDNS
                     (mod.rs, mdns.rs); config-selected endpoints — Camera (WebRTC
                     Transport/Camera AV Stream/Zone Management, camera.rs, live
                     H.264 encode tap in encoder.rs), On/Off Light/Switch (real
                     GPIO output, onoff.rs), and a full color light (dimming +
                     hue/saturation/XY/color temperature, light.rs, in-memory
                     until real PWM/RGB hardware exists) — all independently on/off
  cluster/           broker-less WiFi/BLE mesh: discovery, leader election,
                     detection fusion (camera + radar), remote-command reactions
  onboarding.rs      QR device onboarding payload + PNG rendering
  stream/            encoder planning + RTSP server + cloud-push relay + overlays +
                     widgets.rs (sparkline/gauge machine-data graphics on video)
  storage/           NVR chunk recorder, event clip extraction, SD/FTPS export
  security.rs        RTSP/ONVIF access control (WS-Security digest)
  commands.rs        MQTT command channel (status/snapshot/clip/export/stream/…)
  telemetry.rs       iiotedge-lib engine bridge (tokio runtime thread, GDE events)
  onvif/             WS-Discovery + Device/Media/PTZ SOAP services
  ptz/               PTZ motor control: driver registry + Pelco-D backend
  snmp/              SNMP v2c agent: hand-rolled BER, MIB-II + private MIB, traps
  identity.rs        hardware-derived device_id (Phase 12, F10)
  footprint.rs       device footprint: model, fw version+git hash, config hash, features
  core/              metrics/health/footprint HTTP server, thread-liveness watchdog,
                     systemd sd_notify integration
build.rs             embeds the build-time git commit hash (footprint's git_hash)
config/              firmware + SDK configuration
config/presets/      21 deployable Industry 4.0 + target-customer-vertical + smart-home + radar-demo config templates
deploy/              systemd unit (Type=notify, WatchdogSec=)
scripts/             .deb packaging (make deb)
Dockerfile.cross     aarch64 cross-build container (arm64 GStreamer sysroot)
Makefile             build / quality gates / dist / deb / deploy / device ops
docs/FEATURES.md     full production feature specification
docs/QR_ONBOARDING.md  mobile-app integration contract for QR onboarding
TODO.md              phased roadmap with status
```

## Development

```bash
make ci            # fmt-check + clippy -D warnings + tests (same as GitHub CI)
make docker-lint   # clippy for the aarch64 target (lints Linux-only HAL code)
make dist          # versioned tarball: binary + config + systemd unit + sha256
```

## Dependencies

The [iiotedge-lib] SDK (persist-first telemetry engine, store-and-forward
buffer, TLS/TPM security, southbound machine drivers) is consumed as-is via
path dependencies at `../iiotedge-lib` — it is a separate repository and is
never modified from here.

## License

MIT — see [LICENSE](LICENSE).

[iiotedge-lib]: https://github.com/iiotedge/iiotedge-lib
