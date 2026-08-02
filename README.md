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
                                                        │     ──► events + fused detections ─┐
                                                        ├─► RTSP server (mpph264enc/…) ──┐    │
                                                        │     └─► optional cloud relay   │    │
                                                        └─► NVR chunk recorder (MP4+rotation) │
   machine data (serial / CAN / Modbus …) ─► iiotedge-lib southbound ─┐                  │    │
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
| ONVIF | ✅ | WS-Discovery + Device/Media SOAP (GetStreamUri, Profiles, …), WS-Security auth |
| NVR local recording | ✅ | Fixed-length MP4 chunks, size/age rotation, independent encoder session |
| SD/USB + FTPS evidence export | ✅ | Mirrors chunks/clips/snapshots off-device, auto or on-demand |
| On-device AI detection | ✅ | RKNN (Rockchip NPU) or ONNX Runtime backend, YOLOv8 parser, configurable class filter |
| Tamper detection | ✅ | Blackout, blinding, occlusion, freeze, scene-change |
| Zone motion detection | ✅ | Configurable zones, cheaper middle ground between "always record" and AI |
| Machine-data correlation | ✅ | Pairs southbound events (serial/CAN/Modbus) with the frame on screen when they arrived |
| On-video overlays | ✅ | Timestamp/device-id burn-in + live detection boxes with label/confidence |
| Cluster mesh | ✅ | Broker-less WiFi multicast / BLE — peer discovery, leader election, cross-device detection fusion, peer-triggered re-analysis, agentic remote-command reactions, `/cluster/status` |
| QR device onboarding | ✅ | Scan-to-pair for the mobile app: connection info, credentials, and API token in one QR code |
| Cloud-push relay | ✅ | On-demand RTSP relay to a cloud media server, LAN-only local stream untouched |
| Telemetry (persist-first) | ✅ | [iiotedge-lib] engine: SQLite WAL buffer → MQTT GDE JSON / Sparkplug B |
| Machine data southbound | ✅ | Serial, CAN/J1939, Modbus TCP — **config-only** in `config/edge.toml` |
| Thread-liveness watchdog | ✅ | Per-worker heartbeats; a wedged thread trips a clean supervised restart |
| Customizable AI detection rules (zones/line-crossing/loitering/workflow actions) | 🔜 | Design complete, see [TODO.md](TODO.md) Phase 16 |
| SNMP / fleet footprint | 🔜 | See [TODO.md](TODO.md) / [docs/FEATURES.md](docs/FEATURES.md) |

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
                     YOLOv8 parser, letterbox preprocessing
  tamper.rs          blackout/blinding/occlusion/freeze/scene-change detection
  motion.rs          zone-based motion detection
  correlation.rs     pairs southbound machine events with the frame on screen
  cluster/           broker-less WiFi/BLE mesh: discovery, leader election,
                     detection fusion, remote-command reactions
  onboarding.rs      QR device onboarding payload + PNG rendering
  stream/            encoder planning + RTSP server + cloud-push relay + overlays
  storage/           NVR chunk recorder, event clip extraction, SD/FTPS export
  security.rs        RTSP/ONVIF access control (WS-Security digest)
  commands.rs        MQTT command channel (status/snapshot/clip/export/stream/…)
  telemetry.rs       iiotedge-lib engine bridge (tokio runtime thread, GDE events)
  onvif/             WS-Discovery + Device/Media SOAP services
  core/              metrics/health HTTP server, thread-liveness watchdog
config/              firmware + SDK configuration
deploy/              systemd unit
Dockerfile.cross     aarch64 cross-build container (arm64 GStreamer sysroot)
Makefile             build / quality gates / dist / deploy / device ops
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
