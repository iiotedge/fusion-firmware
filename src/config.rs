// src/config.rs
//
// Every new field carries a serde default so config files written for older
// firmware versions keep parsing — mass-deployed devices must never brick on
// a config schema bump.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub system: SystemConfig,
    pub camera: CameraConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    pub ai: AiConfig,
    pub stream: StreamConfig,
    #[serde(default)]
    pub onvif: OnvifConfig,
    #[serde(default)]
    pub ptz: PtzConfig,
    #[serde(default)]
    pub snmp: SnmpConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub correlation: CorrelationConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
    #[serde(default)]
    pub tamper: TamperConfig,
    #[serde(default)]
    pub motion: MotionConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub cluster: ClusterConfig,
    #[serde(default)]
    pub cloud_relay: CloudRelayConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    pub telemetry: TelemetryConfig,
    pub watchdog: WatchdogConfig,
    #[serde(default)]
    pub onboarding: OnboardingConfig,
    #[serde(default)]
    pub home_assistant: HomeAssistantConfig,
    #[serde(default)]
    pub mqtt_bridge: Vec<MqttBridgeSource>,
    #[serde(default)]
    pub radar: RadarConfig,
    #[serde(default)]
    pub matter: MatterConfig,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct SystemConfig {
    pub device_id: String,
    pub facility_id: String,
    pub log_level: String,
    pub log_format: String,
    pub queue_capacity: usize,
    /// Prometheus /metrics + /healthz HTTP endpoint.
    #[serde(default = "default_metrics_enabled")]
    pub metrics_enabled: bool,
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
    /// Log a warning edge when SoC temperature crosses this (°C).
    #[serde(default = "default_warn_temp_c")]
    pub warn_temp_c: f64,
    /// Where a hardware-derived device_id (src/identity.rs, Phase 12) is
    /// cached across reboots when `device_id` above is left empty —
    /// ignored entirely once `device_id` is set explicitly, so an existing
    /// deployed config that already assigns one is unaffected.
    #[serde(default = "default_identity_file")]
    pub identity_file: String,
    /// Where a remotely-applied `ai.rules` change (MQTT `config_set_ai_rules`
    /// / `POST /config/ai-rules`, src/runtime_config.rs) persists across
    /// reboots — checked on every boot and, if present and still valid,
    /// REPLACES this file's own `[[ai.rules]]` for that run. Same
    /// "persist to disk, re-read at next boot" pattern as `identity_file`
    /// above, applied to a richer payload than a single string.
    #[serde(default = "default_ai_rules_override_file")]
    pub ai_rules_override_file: String,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_identity_file() -> String {
    "config/identity.txt".to_string()
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_ai_rules_override_file() -> String {
    "config/ai_rules_override.json".to_string()
}

// serde(default) targets: only called through the derived Deserialize impl's
// generated code, which newer clippy's dead-code check doesn't credit as a
// use in the test-target build (see the -D warnings run in CI).
#[allow(dead_code)]
fn default_warn_temp_c() -> f64 {
    80.0
}

#[allow(dead_code)]
fn default_metrics_enabled() -> bool {
    true
}
#[allow(dead_code)]
fn default_metrics_port() -> u16 {
    9100
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct CameraConfig {
    pub r#type: String,
    pub device_node: String,
    /// Raw `key=value` pairs appended to the capture source element
    /// (GST_V4L2 backend only), e.g. "io-mode=4" for DMABUF import on
    /// Rockchip ISPs. Same escape-hatch pattern as stream.encoder_params.
    #[serde(default)]
    pub source_params: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: String,
    pub auto_exposure: bool,
    pub exposure_time_us: u32,
    pub gain: u32,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct AiConfig {
    pub enabled: bool,
    pub model_path: String,
    /// Inference runtime backend: "onnx" (built-in). Planned backends (rknn,
    /// tensorrt, openvino, hailo, tflite) are recognized with guidance —
    /// see src/ai/runtime.rs.
    #[serde(default = "default_ai_runtime")]
    pub runtime: String,
    /// Advisory accelerator preference, interpreted by the selected backend
    /// (ONNX: maps to execution providers; CPU is always available).
    pub hardware_delegate: String,
    /// Output decoder: "yolov8" (covers v9/v11). See src/ai/parser.rs.
    #[serde(default = "default_ai_parser")]
    pub parser: String,
    pub confidence_threshold: f32,
    #[serde(default = "default_nms_iou")]
    pub nms_iou_threshold: f32,
    pub inference_fps_limit: u32,
    /// [] = full frame, or [x, y, w, h] analysis window.
    pub roi: Vec<u32>,
    /// Model input resolution (letterboxed).
    #[serde(default = "default_ai_input")]
    pub input_width: u32,
    #[serde(default = "default_ai_input")]
    pub input_height: u32,
    /// Class-id → label names for the loaded model ([] = "class_<id>").
    #[serde(default)]
    pub labels: Vec<String>,
    /// Report only these labels ([] = all).
    #[serde(default)]
    pub class_filter: Vec<String>,
    /// CPU threads for the inference runtime.
    #[serde(default = "default_intra_threads")]
    pub intra_threads: usize,
    /// Linux builds dlopen ONNX Runtime; resolution order: this path →
    /// ORT_DYLIB_PATH env → "libonnxruntime.so" on the loader path.
    /// Ignored on dev hosts (statically linked there).
    #[serde(default)]
    pub onnx_dylib_path: String,
    /// Enables the `test_detect` command (commands.rs), which injects a
    /// synthetic AI detection (fusion record + cluster ai_event broadcast)
    /// through the exact same code path a genuine detection uses. Off by
    /// default and meant to stay off on any device with a real camera —
    /// it exists so a mock-camera test cluster (no real photographic
    /// input, so genuine COCO inference never fires) can still reliably
    /// exercise cross-device detection fusion end-to-end.
    #[serde(default)]
    pub test_hooks_enabled: bool,
    /// Customizable detection rules — zone presence, line crossing,
    /// loitering (TODO.md Phase 16). See src/ai/rules.rs.
    #[serde(default)]
    pub rules: Vec<AiRule>,
}

/// One customizable AI detection rule (`[[ai.rules]]`) — zone presence,
/// line crossing, or loitering. Sits strictly downstream of
/// `confidence_threshold`/`class_filter` above: src/ai/rules.rs only ever
/// sees detections that already passed those, it doesn't replace them.
// PartialEq (Phase 20, src/runtime_config.rs): lets a remote rule-set
// update be compared against what's already in effect, so a no-op
// resubmission can skip rebuilding the live RuleEngine — rebuilding
// resets every in-flight loiter dwell timer / line-cross side state
// (src/ai/rules.rs's CompiledRule.tracks), so doing it on a genuinely
// unchanged rule set would be a silent correctness bug, not just waste.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct AiRule {
    pub name: String,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
    /// Subset of `ai.labels` this rule applies to; empty = any class.
    #[serde(default)]
    pub classes: Vec<String>,
    /// Raises (never lowers) the effective confidence floor for this rule
    /// only — a detection below `ai.confidence_threshold` never reaches
    /// this rule at all, so a `min_confidence` below the global threshold
    /// has no effect. `None` = inherit the global threshold as-is.
    #[serde(default)]
    pub min_confidence: Option<f32>,
    /// Normalized `[0,1]` points: 2 = a line (`line_cross` mode), 3+ = a
    /// polygon (`presence`/`loiter` mode).
    pub zone: Vec<[f32; 2]>,
    /// "presence" | "line_cross" | "loiter"
    pub mode: String,
    /// `line_cross` only: "a_to_b" | "b_to_a" | "either"
    #[serde(default)]
    pub direction: String,
    /// `loiter` only: seconds inside the zone before the rule fires.
    #[serde(default)]
    pub dwell_s: u64,
    /// Rule is armed only during these windows — same day/time format as
    /// `[schedule]`; the type's own default (`enabled=false`) means
    /// "always armed."
    #[serde(default)]
    pub schedule: ScheduleConfig,
    /// "snapshot" | "clip" | "cluster_broadcast" | "webhook" | "gpio_output"
    /// — telemetry publish happens on every match regardless of this list.
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub gpio_chip: String,
    #[serde(default)]
    pub gpio_line: u32,
    #[serde(default = "default_gpio_pulse_ms")]
    pub gpio_pulse_ms: u64,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_rule_enabled() -> bool {
    true
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_gpio_pulse_ms() -> u64 {
    500
}

fn default_ai_runtime() -> String {
    "onnx".to_string()
}
fn default_ai_parser() -> String {
    "yolov8".to_string()
}
fn default_nms_iou() -> f32 {
    0.45
}
fn default_ai_input() -> u32 {
    640
}
fn default_intra_threads() -> usize {
    2
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct StreamConfig {
    pub enabled: bool,
    /// "h264" | "h265" (aliases: "avc", "hevc"). Selects parser/payloader and
    /// which encoder candidate list is searched.
    #[serde(default = "default_codec")]
    pub codec: String,
    /// "auto" probes `encoder_candidates_*` in order and picks the first
    /// element present on the device. Any other value names an explicit
    /// GStreamer element (e.g. "mpph265enc", "vpuenc_hevc", "x264enc") which
    /// is preferred but still falls back to the candidates if missing.
    pub encoder: String,
    /// Raw `key=value` pairs appended to the encoder in the launch string —
    /// the escape hatch for element-specific tuning. Later values override
    /// the generated ones.
    #[serde(default)]
    pub encoder_params: String,
    /// Probed in order for codec = "h264". Defaults cover Rockchip MPP
    /// (Radxa Zero 3E), NXP VPU (i.MX 8M Plus), V4L2 stateful, Apple
    /// VideoToolbox (dev Macs), then software.
    #[serde(default = "default_h264_candidates")]
    pub encoder_candidates_h264: Vec<String>,
    /// Probed in order for codec = "h265".
    #[serde(default = "default_h265_candidates")]
    pub encoder_candidates_h265: Vec<String>,
    pub bitrate_kbps: u32,
    pub gop_size: u32,
    pub rtsp_port: u16,
    pub rtsp_path: String,
    pub osd_overlay_enabled: bool,
    /// `gst_rtsp_media_factory_set_latency()` — ms of jitter-buffer slack the
    /// RTSP server holds before releasing data downstream. GStreamer's own
    /// default is 200ms; that alone is usually the single biggest source of
    /// "stream delay" complaints on an RTSP pipeline that is otherwise
    /// correctly tuned. 0 = no added jitter-buffer delay (recommended for a
    /// stable LAN/production network); raise it only if a lossy link causes
    /// visible stutter.
    #[serde(default)]
    pub rtsp_latency_ms: u32,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_codec() -> String {
    "h264".to_string()
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_h264_candidates() -> Vec<String> {
    [
        "mpph264enc",  // Rockchip MPP (RK3566 — Radxa Zero 3E)
        "vpuenc_h264", // NXP i.MX 8M Plus VPU
        "v4l2h264enc", // V4L2 stateful encoders (mainline kernels)
        "vtenc_h264",  // Apple VideoToolbox (macOS dev machines)
        "x264enc",     // software fallback
        "openh264enc", // software fallback
    ]
    .map(String::from)
    .to_vec()
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_h265_candidates() -> Vec<String> {
    [
        "mpph265enc",  // Rockchip MPP (RK3566 — Radxa Zero 3E)
        "vpuenc_hevc", // NXP i.MX 8M Plus VPU
        "v4l2h265enc", // V4L2 stateful encoders
        "vtenc_h265",  // Apple VideoToolbox (macOS dev machines)
        "x265enc",     // software fallback
    ]
    .map(String::from)
    .to_vec()
}

/// Microphone capture muxed into RTSP and recordings (F1 audio).
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct AudioConfig {
    pub enabled: bool,
    /// "auto" (system default mic) or any GStreamer source fragment:
    /// "alsasrc device=hw:1,0", "audiotestsrc is-live=true" (bench tone), …
    pub source: String,
    /// "opus" (built-in everywhere) | "aac" (encoder probed at runtime).
    pub codec: String,
    pub bitrate_kbps: u32,
    pub sample_rate: u32,
    pub channels: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: "auto".to_string(),
            codec: "opus".to_string(),
            bitrate_kbps: 64,
            sample_rate: 48000,
            channels: 1,
        }
    }
}

/// Access control for the camera's network services (F2/F8 hardening).
/// A shared user store gates RTSP and ONVIF; the command channel is gated by
/// a bearer token and rides the telemetry TLS. Empty/disabled = open (the
/// pre-hardening behavior), logged loudly at boot.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct SecurityConfig {
    /// Require credentials for RTSP playback (basic/digest).
    pub rtsp_auth: bool,
    /// Require WS-UsernameToken on ONVIF SOAP calls.
    pub onvif_auth: bool,
    /// Shared RTSP/ONVIF user store. Empty ⇒ anonymous access.
    pub users: Vec<UserConfig>,
    /// Bearer token required in command payloads (`"token": "..."`). Empty ⇒
    /// commands are unauthenticated (only sensible on a private broker).
    pub command_token: String,
    /// Bearer token (`Authorization: Bearer <token>`) required on the
    /// mobile-app-facing HTTP API (`/cluster/status`, see core/metrics.rs).
    /// Deliberately separate from `command_token`: this is the credential
    /// handed to end-user mobile apps via QR onboarding (src/onboarding.rs),
    /// while `command_token` stays known only to installers/cluster peers —
    /// a compromised phone should never be able to issue commands. Empty ⇒
    /// unauthenticated (only sensible on a private LAN).
    pub api_token: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct UserConfig {
    pub username: String,
    pub password: String,
    /// "admin" | "viewer" — advisory today (both may view); reserved for
    /// per-role permissions (PTZ, config) later.
    #[serde(default = "d_role")]
    pub role: String,
}

fn d_role() -> String {
    "viewer".to_string()
}

/// QR-code device onboarding (Phase 12): serves a JSON payload + PNG QR code
/// (src/onboarding.rs) carrying everything the mobile app needs to add this
/// camera without manual IP/credential entry — device_id, LAN host, ONVIF/
/// RTSP/metrics ports, RTSP+ONVIF credentials (from [security].users) and
/// the `[security].api_token` the app should present on subsequent calls.
/// Gated by `[security].command_token` (see onboarding.rs) — an installer
/// who already knows that secret can view/print the QR; the mobile app
/// itself never needs to know it.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct OnboardingConfig {
    pub enabled: bool,
    /// Report this instead of `[stream].rtsp_port`/`[onvif].port`/
    /// `[system].metrics_port` in the onboarding payload. `None` (the
    /// common case) reports the real listen port unchanged — only set
    /// these when this device sits behind port-forwarding/NAT and the
    /// externally-reachable port differs from what the process itself
    /// binds (e.g. this project's own Docker test cluster, where several
    /// simulated nodes share one host and Docker remaps each node's ports;
    /// see iiotedge-cluster-sim's compose file for a worked example).
    pub external_rtsp_port: Option<u16>,
    pub external_onvif_port: Option<u16>,
    pub external_metrics_port: Option<u16>,
}

impl Default for OnboardingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            external_rtsp_port: None,
            external_onvif_port: None,
            external_metrics_port: None,
        }
    }
}

/// Multi-camera cluster coordination (F9): peer discovery + event sharing
/// with NO cloud/broker dependency — see src/cluster/. Both transports below
/// can run together (WiFi primary, Bluetooth fallback) or independently;
/// enabling neither leaves the camera running standalone exactly as before.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ClusterConfig {
    pub enabled: bool,
    /// Leader-election weight (see cluster::ClusterView::is_leader). Higher
    /// wins; break ties yourself by setting distinct values per device.
    pub priority: u32,
    pub announce_interval_s: u64,
    pub wifi: ClusterWifiConfig,
    pub bluetooth: ClusterBluetoothConfig,
    /// Local reactions to peer events (e.g. "peer tamper alarm → snapshot
    /// here too"). Empty = observe the cluster bus but take no local action.
    pub reactions: Vec<ClusterReaction>,
    /// Whether this device executes commands another cluster peer sends it
    /// (see cluster::MessageKind::Command). Off by default — joining the
    /// mesh (seeing peers, sharing events) is a materially lower-risk
    /// decision than letting any peer remote-control this device, so this
    /// is a separate opt-in from `enabled`, not implied by it.
    pub accept_remote_commands: bool,
    /// Cross-device AI detection fusion (see cluster::fusion): correlates
    /// this device's own recent AI detections against peers' `ai_event`
    /// broadcasts — the same object crossing multiple camera FOVs within
    /// `tolerance_ms` becomes corroborating evidence, not two disconnected
    /// events.
    pub fusion: ClusterFusionConfig,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            priority: 100,
            announce_interval_s: 10,
            wifi: ClusterWifiConfig::default(),
            bluetooth: ClusterBluetoothConfig::default(),
            reactions: Vec::new(),
            accept_remote_commands: false,
            fusion: ClusterFusionConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ClusterFusionConfig {
    pub enabled: bool,
    /// How close together two devices' detections of the same label need to
    /// be to count as the same real-world object, not coincidence (mirrors
    /// `[correlation].tolerance_ms`'s same "same moment" judgment call).
    pub tolerance_ms: u64,
}

impl Default for ClusterFusionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tolerance_ms: 3000,
        }
    }
}

/// Cloud-push relay (stream_start/stream_stop over the existing MQTT
/// command channel): on command, republishes the local RTSP feed to an
/// external RTSP ingest (media-ingestion-service's MediaMTX) via an
/// in-process GStreamer pipeline. Off by default — the local RTSP server
/// (LAN-only pull) works with this disabled; this is a separate, additive
/// capability an operator opts into.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(default)]
pub struct CloudRelayConfig {
    pub enabled: bool,
    /// WebRTC (WHIP) relay mode's STUN server, `stun://host:port` — empty
    /// = `whipclientsink`'s own default (Google's public STUN,
    /// `stun://stun.l.google.com:19302`). STUN only negotiates NAT
    /// traversal (discovers this device's public IP/port); no video ever
    /// passes through it, so the public default is fine for most sites.
    #[serde(default)]
    pub stun_server: String,
    /// WebRTC (WHIP) relay mode's TURN relay, `turn(s)://user:pass@host:port`
    /// — empty = none configured. Only needed behind a symmetric/strict
    /// NAT where STUN alone can't establish a direct path; unlike STUN,
    /// TURN DOES relay the actual media, so this is a real operational
    /// dependency to run/trust, not a free default.
    #[serde(default)]
    pub turn_server: String,
}

/// Radar sensing (TODO.md Phase 17) — off by default, additive alongside
/// the camera (a radar zone violation can trigger a camera snapshot, and
/// vice versa, once 17c fusion lands; today they run standalone). `r#type`
/// selects the backend the same way `[camera].type` does (registry in
/// `src/radar/mod.rs`) — `"mock"` is the only backend shipped today; real
/// vendor hardware (TI mmWave, Continental ARS408, Navtech, ...) needs a
/// new backend module, see that file's header comment for why none exist
/// yet (no hardware/captured traffic to verify a wire-protocol parser
/// against).
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct RadarConfig {
    pub enabled: bool,
    pub r#type: String,
    /// Backend-specific connection string: a CAN interface name ("can0"),
    /// a serial device ("/dev/ttyUSB0"), or a "udp://host:port"/
    /// "tcp://host:port" endpoint — same free-form escape-hatch pattern
    /// as `camera.source_params`, interpreted only by the selected backend.
    pub transport: String,
    pub baud_rate: u32,
    pub range_min_m: f32,
    pub range_max_m: f32,
    /// Total azimuth field of view, degrees (the unit's native FOV, not
    /// a crop) — e.g. 120.0 for a typical wide-FOV industrial radar.
    pub fov_deg: f32,
    /// Mounting pose (extrinsic reference frame): meters/degrees from an
    /// arbitrary site origin — same convention Phase 14's LiDAR design
    /// specified so radar/LiDAR/camera extrinsics stay comparable once
    /// cross-sensor calibration (17c) lands.
    pub mount_x_m: f32,
    pub mount_y_m: f32,
    pub mount_z_m: f32,
    pub mount_roll_deg: f32,
    pub mount_pitch_deg: f32,
    pub mount_yaw_deg: f32,
    #[serde(default)]
    pub zones: Vec<RadarZone>,
}

impl Default for RadarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            r#type: "mock".to_string(),
            transport: String::new(),
            baud_rate: 115_200,
            range_min_m: 0.5,
            range_max_m: 50.0,
            fov_deg: 120.0,
            mount_x_m: 0.0,
            mount_y_m: 0.0,
            mount_z_m: 0.0,
            mount_roll_deg: 0.0,
            mount_pitch_deg: 0.0,
            mount_yaw_deg: 0.0,
            zones: Vec::new(),
        }
    }
}

/// Matter protocol support (Phase 19c, src/matter/): commissions this
/// device into a Matter fabric (Apple Home, Google Home, Alexa,
/// SmartThings, Home Assistant's Matter server, …) as a Camera device
/// exposing WebRTC Transport Provider, Camera AV Stream Management, and
/// Zone Management clusters. Off by default — enabling it opens a UDP+TCP
/// listener and, until commissioned, an open pairing window.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MatterConfig {
    pub enabled: bool,
    /// Where Matter's fabric/ACL/subscription state persists across
    /// reboots. Deliberately NOT a system temp directory (rs-matter's own
    /// default) — that gets cleared on most Linux distros' reboot, which
    /// would silently force re-commissioning after every restart. Same
    /// "relative, config-owned persistence path" convention as
    /// `system.identity_file`/`system.ai_rules_override_file`.
    #[serde(default = "default_matter_state_dir")]
    pub state_dir: String,
    /// No real CSA device-attestation certificate exists for this
    /// firmware yet (same "for now" call as Phase 12c's onboarding QR),
    /// so commissioning always uses rs-matter's own `TEST_DEV_ATT` /
    /// `TEST_DEV_COMM` / `TEST_DEV_DET` — the same constants `chip-tool`
    /// (the reference Matter controller CLI) expects out of the box.
    /// This field exists to make that fact discoverable from the config
    /// file itself rather than only from source comments; it does not
    /// yet change behavior (there is nothing else to switch it to).
    #[serde(default = "default_matter_attestation")]
    pub attestation: String,
}

fn default_matter_state_dir() -> String {
    "config/matter_state".to_string()
}

fn default_matter_attestation() -> String {
    "test".to_string()
}

impl Default for MatterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            state_dir: default_matter_state_dir(),
            attestation: default_matter_attestation(),
        }
    }
}

/// One radar zone rule (`[[radar.zones]]`) — deliberately the same mode
/// vocabulary as `AiRule` (`[[ai.rules]]`, Phase 16): `"presence"` |
/// `"line_cross"` | `"loiter"`, one set of analytics concepts reused
/// across every engine in this firmware, not reinvented per sensor.
/// Coordinates are real-world METERS in the radar's own top-down x,y
/// plane (`radar::polar_to_xy`), NOT the normalized `[0,1]` image-frame
/// fractions `ai.rules`/`motion.zones` use — radar has no image frame to
/// normalize against.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct RadarZone {
    pub name: String,
    #[serde(default = "default_radar_zone_enabled")]
    pub enabled: bool,
    /// `"presence"` | `"line_cross"` | `"loiter"`
    pub mode: String,
    /// `[x_m, y_m]` pairs: 2 = a line (`line_cross`), 3+ = a polygon
    /// (`presence`/`loiter`) — the same "one geometry field covers both
    /// shapes" choice `AiRule.zone` already made.
    pub points: Vec<[f32; 2]>,
    /// `line_cross` only: "a_to_b" | "b_to_a" | "either"
    #[serde(default)]
    pub direction: String,
    /// `loiter` only: seconds inside the zone before the rule fires.
    #[serde(default)]
    pub dwell_s: u64,
    /// Optional minimum radar cross-section (dBsm) — filters out small/
    /// weak returns (birds, blowing debris) below a size threshold,
    /// radar's equivalent of `ai.rules`' `min_confidence`. Not yet
    /// consumed by `src/radar/analytics.rs` (validated, wired in when a
    /// real backend's RCS calibration is available to tune against).
    #[serde(default)]
    pub min_rcs_dbsm: Option<f32>,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_radar_zone_enabled() -> bool {
    true
}

/// Broker-less UDP multicast on the local WiFi/Ethernet segment — no
/// internet, no MQTT broker, works on an isolated site network.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ClusterWifiConfig {
    pub enabled: bool,
    /// Multicast group address (224.0.0.0/4). Keep this off the
    /// well-known/reserved ranges other protocols use on the LAN.
    pub multicast_group: String,
    pub port: u16,
    /// Multicast TTL/hop-limit: 1 = this LAN segment only (recommended
    /// default — cluster traffic should not cross routers).
    pub ttl: u32,
}

impl Default for ClusterWifiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            multicast_group: "239.255.77.77".to_string(),
            port: 8778,
            ttl: 1,
        }
    }
}

/// BLE advertising beacon transport (BlueZ, Linux-only) — for sites with no
/// WiFi network reachable at all. See src/cluster/bluetooth.rs for the
/// payload-size tradeoff this design makes.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ClusterBluetoothConfig {
    pub enabled: bool,
    /// BlueZ adapter name, e.g. "hci0". Empty = system default adapter.
    pub adapter: String,
    pub scan_interval_ms: u64,
}

impl Default for ClusterBluetoothConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            adapter: String::new(),
            scan_interval_ms: 1000,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct ClusterReaction {
    /// Match peer event kind ("tamper_event", "motion_event", "ai_event", …;
    /// "*" matches any).
    pub on_kind: String,
    pub snapshot: bool,
    /// If set, also sends this command to `remote_target` when this
    /// reaction matches — the same command set the MQTT channel accepts
    /// (see commands::handle_command's `other =>` arm for the full list).
    /// This is what makes cross-device automation config-driven: e.g.
    /// "if tamper on any peer, tell it to snapshot" is one reaction block,
    /// not custom code. Requires the RECEIVING device to have
    /// `[cluster].accept_remote_commands = true`.
    #[serde(default)]
    pub remote_cmd: Option<String>,
    /// Which peer `remote_cmd` targets: a specific node_id, or "*" for
    /// every peer. Ignored if `remote_cmd` is unset.
    #[serde(default = "default_remote_target")]
    pub remote_target: String,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_remote_target() -> String {
    "*".to_string()
}

/// Machine-data ↔ video correlation (F7): rules matched against southbound
/// machine events inside the telemetry engine's ingest path; hits pair with
/// the current frame on the analytics thread (timestamps compared against
/// tolerance_ms) and fire snapshot/clip/GDE-event evidence actions.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct CorrelationConfig {
    pub enabled: bool,
    /// Machine-vs-frame timestamp delta considered "same moment".
    pub tolerance_ms: u64,
    pub rules: Vec<CorrelationRule>,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tolerance_ms: 200,
            rules: Vec::new(), // no rules = inert
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct CorrelationRule {
    /// Event name; becomes part of the correlated event and clip/snapshot reason.
    pub name: String,
    /// Match machine events whose source id starts with this
    /// (e.g. "serial/scanner1", "canbus/vehicle", "modbus/plc1/reject_flag").
    pub source_prefix: String,
    /// Optional payload substring filter ("" = any payload).
    #[serde(default)]
    pub contains: String,
    #[serde(default = "d_true")]
    pub snapshot: bool,
    #[serde(default = "d_true")]
    pub clip: bool,
}

fn d_true() -> bool {
    true
}

/// Home Assistant MQTT Discovery (Phase 19a) — publishes retained discovery
/// config messages on the SAME broker/identity as the command channel
/// (edge.toml's [northbound.mqtt]), not a separate broker: HA's button
/// entities need their command_topic to land on the exact topic
/// src/commands.rs already subscribes to for "press a button in HA, it
/// runs a real command" to work with no new command-ingestion path.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct HomeAssistantConfig {
    pub enabled: bool,
    #[serde(default = "d_ha_discovery_prefix")]
    pub discovery_prefix: String,
    /// Device name shown in HA's device registry (groups every entity
    /// this firmware publishes under one device card). Empty = system.device_id.
    #[serde(default)]
    pub device_name: String,
    /// Seconds an ai.rules/tamper/motion binary_sensor stays "on" after
    /// its last trigger before HA auto-resets it to "off" (MQTT
    /// binary_sensor's `off_delay`) — these are momentary detections, not
    /// sustained state, so auto-reset is the correct HA-idiomatic mapping.
    #[serde(default = "d_ha_off_delay_s")]
    pub off_delay_s: u32,
}

impl Default for HomeAssistantConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discovery_prefix: d_ha_discovery_prefix(),
            device_name: String::new(),
            off_delay_s: d_ha_off_delay_s(),
        }
    }
}

fn d_ha_discovery_prefix() -> String {
    "homeassistant".to_string()
}

fn d_ha_off_delay_s() -> u32 {
    10
}

/// Generic MQTT-JSON southbound bridge (Phase 19b) — subscribes to an
/// existing MQTT-based bridge (Zigbee2MQTT, Z-Wave JS UI, or any other
/// JSON-over-MQTT source) and feeds matching events into the SAME
/// [[correlation.rules]] engine already wired to industrial southbound
/// tags (serial/CAN/Modbus), NOT a new evidence/action mechanism.
/// Deliberately not a native Zigbee/Z-Wave radio stack — no mature Rust
/// crate at production quality, no radio hardware on the reference
/// device, and Zigbee2MQTT/Z-Wave JS UI are already the de facto standard
/// bridges most Home-Assistant-adjacent sites already run.
///
/// The correlation `source_id` for every event this bridge delivers is
/// the raw MQTT topic it arrived on (e.g. "zigbee2mqtt/front_door"), so
/// [[correlation.rules]] `source_prefix` matches it exactly the same way
/// it already matches "serial/scanner1" or "modbus/plc1/reject_flag" —
/// no separate prefix field needed here.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct MqttBridgeSource {
    /// Label used only in logs.
    pub name: String,
    pub host: String,
    #[serde(default = "d_mqtt_bridge_port")]
    pub port: u16,
    /// MQTT subscription filter, e.g. "zigbee2mqtt/#" (Zigbee2MQTT) or
    /// "zwave/#" (Z-Wave JS UI gateway mode).
    pub topic_filter: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

fn d_mqtt_bridge_port() -> u16 {
    1883
}

/// Zone motion detection (classic NVR trigger): frame-to-frame luma change
/// inside configured zones, debounced. Cheaper than AI, the standard middle
/// ground between "always record" and "AI detection". Fires motion_event and
/// can gate recording (storage.record_mode).
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct MotionConfig {
    pub enabled: bool,
    pub analysis_fps: u32,
    /// Mean per-cell luma change above which a zone is "moving".
    pub sensitivity: f32,
    /// Consecutive analyzed frames over threshold before motion is declared.
    pub alarm_after_frames: u32,
    /// Motion must be clear this long before it ends.
    pub recover_after_s: u32,
    /// Detection zones (normalized 0..1 rectangles). Empty = whole frame.
    pub zones: Vec<MotionZone>,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            analysis_fps: 5,
            sensitivity: 12.0,
            alarm_after_frames: 2,
            recover_after_s: 3,
            zones: Vec::new(),
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct MotionZone {
    #[serde(default = "d_zone_name")]
    pub name: String,
    /// Normalized [0,1] rectangle within the frame.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

fn d_zone_name() -> String {
    "zone".to_string()
}

/// Weekly arming schedule (shift/calendar). When enabled, "armed" only inside
/// the configured windows (device local time); used to gate recording and/or
/// analytics (storage.record_mode = "schedule"). No windows = never armed.
#[allow(dead_code)]
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq)]
#[serde(default)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub windows: Vec<ScheduleWindow>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
pub struct ScheduleWindow {
    /// Lowercase day abbreviations: mon tue wed thu fri sat sun.
    pub days: Vec<String>,
    /// "HH:MM" 24h local time.
    pub start: String,
    pub end: String,
}

/// Camera tamper detection (video analytics: blackout, blinding, occlusion,
/// frozen feed, scene change). Conditions must persist `alarm_after_s` before
/// alarming and clear for `recover_after_s` before recovery — debouncing
/// against flicker and people briefly walking through the view.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct TamperConfig {
    pub enabled: bool,
    /// Analysis cadence — tamper statistics are cheap, a few Hz suffices.
    pub analysis_fps: u32,
    pub alarm_after_s: u32,
    pub recover_after_s: u32,
    /// Mean luma below ⇒ blackout / lens covered in the dark.
    pub dark_luma_max: f32,
    /// Mean luma above ⇒ blinding (flashlight/laser attack).
    pub bright_luma_min: f32,
    /// Mean local gradient below ⇒ occlusion/defocus (uniform image).
    pub low_detail_min: f32,
    /// Mean inter-frame difference below ⇒ frozen feed.
    pub freeze_diff_max: f32,
    /// Mean difference vs the slow reference above ⇒ camera moved/repointed.
    pub scene_change_min: f32,
    /// Draw a full-frame warning border while any tamper is active.
    pub overlay_border: bool,
}

impl Default for TamperConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            analysis_fps: 4,
            alarm_after_s: 3,
            recover_after_s: 5,
            dark_luma_max: 30.0,
            bright_luma_min: 225.0,
            low_detail_min: 1.5,
            freeze_diff_max: 0.2,
            scene_change_min: 60.0,
            overlay_border: true,
        }
    }
}

/// Burned-in OSD overlays (Industry 4.0 evidence trail: every frame carries
/// its provenance). Text overlays render via GStreamer's pango elements when
/// present; AI boxes are drawn CPU-side into the shared frame so RTSP and the
/// NVR recorder show identical evidence. Supersedes stream.osd_overlay_enabled.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct OverlayConfig {
    pub enabled: bool,
    /// Wall-clock burn-in (clockoverlay), top-right.
    pub show_timestamp: bool,
    /// strftime format for the timestamp.
    pub timestamp_format: String,
    /// Device id burn-in (textoverlay), top-left.
    pub show_device_id: bool,
    /// Free text (bottom-left); empty = none. E.g. "LINE 1 — INSPECTION".
    pub custom_text: String,
    /// Pango font description for all text overlays.
    pub font: String,
    /// Draw AI detection boxes + labels state into the video.
    pub show_ai_boxes: bool,
    /// How long a detection stays on screen after its inference (ms).
    pub ai_box_ttl_ms: u64,
    /// Realtime machine-data widgets rendered onto the video (sparkline
    /// trends, bar gauges, live values) — bound to telemetry source ids.
    pub widgets: Vec<WidgetConfig>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_timestamp: true,
            timestamp_format: "%Y-%m-%d %H:%M:%S".to_string(),
            show_device_id: true,
            custom_text: String::new(),
            font: "Sans, 18".to_string(),
            show_ai_boxes: true,
            ai_box_ttl_ms: 1500,
            widgets: Vec::new(),
        }
    }
}

/// One on-video machine-data widget (see stream::widgets).
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct WidgetConfig {
    /// "sparkline" (trend graph) | "gauge" (bar) | "value" (big number).
    pub kind: String,
    /// Short caption drawn in the widget header (5×7 bitmap font: A–Z 0–9 . - : % /).
    #[serde(default)]
    pub label: String,
    /// Telemetry source-id prefix to bind, e.g. "modbus/plc1/boiler_temp"
    /// or "camera/<device_id>/health" for the firmware's own beacons.
    pub source: String,
    /// Extract this numeric field from JSON payloads ("" = payload is the number).
    #[serde(default)]
    pub json_field: String,
    /// Scale for gauge/sparkline; leave min == max to auto-scale sparklines.
    #[serde(default)]
    pub min: f64,
    #[serde(default)]
    pub max: f64,
    pub x: u32,
    pub y: u32,
    #[serde(default = "d_widget_w")]
    pub width: u32,
    #[serde(default = "d_widget_h")]
    pub height: u32,
    /// Sparkline history window.
    #[serde(default = "d_widget_window")]
    pub window_s: u64,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn d_widget_w() -> u32 {
    280
}
#[allow(dead_code)] // serde(default) target, not hand-called
fn d_widget_h() -> u32 {
    100
}
#[allow(dead_code)] // serde(default) target, not hand-called
fn d_widget_window() -> u64 {
    120
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct OnvifConfig {
    pub enabled: bool,
    /// HTTP port for the SOAP device/media services
    /// (http://<ip>:<port>/onvif/device_service).
    pub port: u16,
    /// Answer WS-Discovery probes on UDP 3702 so VMS/NVR software finds the
    /// camera without manual IP entry.
    pub discovery_enabled: bool,
    pub manufacturer: String,
    pub model: String,
    pub hardware_id: String,
}

impl Default for OnvifConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8000,
            discovery_enabled: true,
            manufacturer: "IIoTEdge".to_string(),
            model: "IIoTEdge Vision Node".to_string(),
            hardware_id: "radxa-zero-3e".to_string(),
        }
    }
}

/// PTZ (pan/tilt/zoom) motor control, off by default — most deployments are
/// fixed cameras. Exposed to clients via the ONVIF PTZ service
/// (src/onvif/services.rs) and the MQTT command channel (ptz_move/
/// ptz_preset), both dispatching through one `PtzController` (src/ptz/).
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct PtzConfig {
    pub enabled: bool,
    /// "pelco_d" today (src/ptz/pelco_d.rs). "onvif_passthrough" is planned
    /// once the rtsp_in HAL proxy backend lands (TODO.md Phase 2) — same
    /// registry pattern as the camera HAL (src/ptz/mod.rs), so adding it
    /// later is a new backend module, not a rewrite of this config or the
    /// ONVIF/MQTT call sites.
    pub driver: String,
    /// Serial device node, e.g. "/dev/ttyUSB0" (USB-RS485 adapter) or
    /// "/dev/ttyAMA0" (onboard UART). Required when enabled.
    pub serial_device: String,
    pub baud_rate: u32,
    /// Pelco-D device address (0-255) — lets multiple PTZ units share one
    /// RS-485 bus, each answering only its own address.
    pub address: u8,
    /// Safety timeout: ONVIF ContinuousMove keeps a mechanism moving until
    /// an explicit Stop arrives. A client that crashes or drops connection
    /// mid-move would otherwise leave it moving indefinitely — the driver
    /// auto-stops after this many seconds with no Stop/refresh.
    pub move_timeout_s: u64,
}

impl Default for PtzConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            driver: "pelco_d".to_string(),
            serial_device: String::new(),
            baud_rate: 2400,
            address: 0,
            move_timeout_s: 5,
        }
    }
}

/// SNMP agent (TODO.md Phase 12, F10) — off by default. v2c only (v3's
/// USM auth/privacy is real added complexity; not built, see src/snmp/).
/// MIB-II system group + a private enterprise MIB (streams/tamper/storage)
/// backed by the same counters `/metrics` already tracks (src/core/metrics.rs)
/// — one source of truth, not a second parallel metrics pipeline.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SnmpConfig {
    pub enabled: bool,
    /// Standard SNMP port is 161 (privileged — the systemd unit grants
    /// CAP_NET_BIND_SERVICE for exactly this). Set to something >1024 for
    /// a dev host or a deployment that doesn't want the extra capability.
    pub port: u16,
    /// SNMPv2c community string gates every request — anyone who knows it
    /// can read every OID this agent exposes. Treat it like a password,
    /// not a public label (v2c has no encryption, so it's still sent in
    /// the clear on the wire — fine for a trusted management VLAN, not a
    /// substitute for network-level access control).
    pub community: String,
    pub sys_contact: String,
    pub sys_location: String,
    /// Trap receiver for critical events (tamper alarm today — see
    /// src/snmp/mod.rs's send_trap call site). Empty = traps disabled.
    pub trap_host: String,
    pub trap_port: u16,
}

impl Default for SnmpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 161,
            community: "public".to_string(),
            sys_contact: String::new(),
            sys_location: String::new(),
            trap_host: String::new(),
            trap_port: 162,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct TelemetryConfig {
    pub enabled: bool,
    /// Path to the iiotedge-lib EdgeConfig TOML that drives the whole
    /// persist-first pipeline: SQLite store-and-forward buffer, northbound
    /// transport (MQTT GDE/Sparkplug B + TLS/TPM) and southbound machine
    /// drivers (serial, CAN, Modbus, OPC UA, …). See config/edge.toml.
    #[serde(default = "default_edge_config")]
    pub edge_config: String,
    /// Interval for the firmware's own health event on `camera/<id>/health`.
    pub health_ping_interval_sec: u64,
    // --- Legacy keys (superseded by edge_config; parsed so old fleet
    // --- configs keep loading, ignored by the engine) ---
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub mqtt_broker: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub qos: u8,
    #[serde(default)]
    pub topic_events: String,
    #[serde(default)]
    pub topic_health: String,
}

#[allow(dead_code)] // serde(default) target, not hand-called
fn default_edge_config() -> String {
    "config/edge.toml".to_string()
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct StorageConfig {
    pub enabled: bool,
    /// Directory for NVR-style recording chunks (created if missing).
    pub path: String,
    /// Length of each recording chunk. Industry-typical: 30–300 s.
    pub chunk_seconds: u32,
    /// Rotation cap: oldest chunks are deleted once the directory exceeds
    /// this size (the chunk currently being written is never deleted).
    pub max_total_mb: u64,
    /// Additionally drop chunks older than this many hours (0 = no age cap).
    pub max_age_hours: u64,
    /// Save JPEG snapshots on AI/tamper events (into <path>/snapshots/).
    pub snapshots_enabled: bool,
    /// JPEG quality (10–100).
    pub snapshot_quality: u32,
    /// Minimum spacing between event snapshots — an event storm must not
    /// turn the camera into a JPEG factory.
    pub snapshot_min_interval_s: u64,
    /// Event clips: hardlink the chunks covering the pre/post-roll window
    /// into <path>/clips/<event>/ with a JSON manifest (no re-encoding;
    /// bytes survive chunk rotation for free).
    pub clips_enabled: bool,
    pub clip_pre_roll_s: u64,
    pub clip_post_roll_s: u64,
    /// Rotation cap for <path>/clips — whole oldest clips removed first.
    pub max_clips_mb: u64,
    /// Warn (log + metric) when the storage filesystem's free space drops
    /// below this — rotation caps bound OUR usage, not the whole disk's.
    pub min_free_mb: u64,
    /// When to feed the recorder:
    ///   "continuous"           — always (default NVR behavior)
    ///   "motion"               — only while motion is active (+post-roll)
    ///   "schedule"             — only while the schedule is armed
    ///   "motion_and_schedule"  — armed AND motion
    pub record_mode: String,
    /// Keep recording this long after motion clears (motion modes).
    pub motion_post_roll_s: u64,
    /// Mirror evidence to removable SD/USB media.
    pub sd: SdExportConfig,
    /// Upload evidence to a site server over FTPS/FTP.
    pub ftp: FtpExportConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "recordings".to_string(),
            chunk_seconds: 60,
            max_total_mb: 2048,
            max_age_hours: 0,
            snapshots_enabled: true,
            snapshot_quality: 85,
            snapshot_min_interval_s: 5,
            clips_enabled: true,
            clip_pre_roll_s: 30,
            clip_post_roll_s: 30,
            max_clips_mb: 1024,
            min_free_mb: 512,
            record_mode: "continuous".to_string(),
            motion_post_roll_s: 10,
            sd: SdExportConfig::default(),
            ftp: FtpExportConfig::default(),
        }
    }
}

/// SD/USB media export: mirrors chunks/clips/snapshots onto removable media
/// mounted at `mount_path`. `auto` syncs continuously whenever the media is
/// present and writable; a manual full sync fires from the command channel
/// (`cmd=export`) or the optional GPIO button.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct SdExportConfig {
    pub enabled: bool,
    /// Where the media is mounted (fstab/udev handles mounting).
    pub mount_path: String,
    /// Continuous mirroring while media is present ("on insertion" behavior);
    /// false = manual-only (button / command).
    pub auto: bool,
    pub upload_chunks: bool,
    pub upload_clips: bool,
    pub upload_snapshots: bool,
    pub scan_interval_s: u64,
    /// Optional export button (Linux gpio-cdev): chip device and line offset.
    /// Empty chip = no button.
    pub button_gpio_chip: String,
    pub button_gpio_line: u32,
    pub button_active_low: bool,
}

impl Default for SdExportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mount_path: "/media/usb".to_string(),
            auto: true,
            upload_chunks: true,
            upload_clips: true,
            upload_snapshots: true,
            scan_interval_s: 30,
            button_gpio_chip: String::new(),
            button_gpio_line: 0,
            button_active_low: true,
        }
    }
}

/// Secure FTP upload of evidence to a site server. TLS on = explicit FTPS
/// (AUTH TLS, rustls). `insecure_skip_verify` accepts self-signed server
/// certificates — common on factory FTP appliances, logged loudly.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct FtpExportConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub tls: bool,
    pub insecure_skip_verify: bool,
    /// Remote base directory; uploads land under <remote_dir>/<device_id>/…
    pub remote_dir: String,
    pub upload_chunks: bool,
    pub upload_clips: bool,
    pub upload_snapshots: bool,
    pub scan_interval_s: u64,
    /// Free local space after a confirmed upload (chunks only; clips and
    /// snapshots stay for local evidence until rotation).
    pub delete_after_upload: bool,
}

impl Default for FtpExportConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 21,
            username: "anonymous".to_string(),
            password: String::new(),
            tls: true,
            insecure_skip_verify: false,
            remote_dir: "/".to_string(),
            upload_chunks: false,
            upload_clips: true,
            upload_snapshots: true,
            scan_interval_s: 60,
            delete_after_upload: false,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct WatchdogConfig {
    pub thread_timeout_ms: u64,
    pub max_consecutive_dropped_frames: u32,
}

pub fn load_config<P: AsRef<Path>>(path: P) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let config: AppConfig = toml::from_str(&contents)?;
    validate(&config)?;
    Ok(config)
}

// Structural ceilings, not tuning knobs — generous enough that no real
// deployment (config file or remote) ever brushes up against them
// (TODO.md's own presets use 1-3 rules), but a hard backstop against a
// pathological or malicious payload bloating disk/memory/boot-time cost.
// Load-bearing specifically for the remote paths (src/runtime_config.rs's
// `config_set_ai_rules` / `POST /config/ai-rules`) since those are
// network-reachable and, when `[security].command_token` is left empty
// (an existing, documented "open" mode), unauthenticated.
const MAX_AI_RULES: usize = 200;
const MAX_ZONE_POINTS: usize = 64;
const MAX_RULE_NAME_LEN: usize = 128;
const MAX_URL_LEN: usize = 2048;
const MAX_GPIO_CHIP_LEN: usize = 256;

/// Validates a `[[ai.rules]]` list on its own — extracted out of `validate()`
/// below so it's the SAME function both a boot-time TOML load and a runtime
/// remote config change (src/runtime_config.rs's `config_set_ai_rules` /
/// `POST /config/ai-rules`) run through. Nothing settable over the network
/// can ever be more permissive than what a fresh boot from the config file
/// would already accept.
pub fn validate_ai_rules(rules: &[AiRule], labels: &[String]) -> Result<(), String> {
    if rules.len() > MAX_AI_RULES {
        return Err(format!(
            "ai.rules: {} rules exceeds the {MAX_AI_RULES}-rule limit",
            rules.len()
        ));
    }
    for rule in rules {
        let ctx = format!("ai.rules[\"{}\"]", rule.name);
        if rule.name.is_empty() {
            return Err("every ai.rules entry needs a non-empty name".to_string());
        }
        if rule.name.len() > MAX_RULE_NAME_LEN {
            return Err(format!(
                "{ctx}: name exceeds {MAX_RULE_NAME_LEN} characters"
            ));
        }
        if rule.zone.len() > MAX_ZONE_POINTS {
            return Err(format!(
                "{ctx}: zone has {} points, exceeding the {MAX_ZONE_POINTS}-point limit",
                rule.zone.len()
            ));
        }
        if rule.webhook_url.len() > MAX_URL_LEN {
            return Err(format!(
                "{ctx}: webhook_url exceeds {MAX_URL_LEN} characters"
            ));
        }
        if rule.gpio_chip.len() > MAX_GPIO_CHIP_LEN {
            return Err(format!(
                "{ctx}: gpio_chip exceeds {MAX_GPIO_CHIP_LEN} characters"
            ));
        }
        match rule.mode.as_str() {
            "presence" | "loiter" if rule.zone.len() < 3 => {
                return Err(format!(
                    "{ctx}: mode \"{}\" needs a polygon (>=3 points), got {}",
                    rule.mode,
                    rule.zone.len()
                ));
            }
            "line_cross" if rule.zone.len() != 2 => {
                return Err(format!(
                    "{ctx}: mode \"line_cross\" needs exactly 2 points, got {}",
                    rule.zone.len()
                ));
            }
            "presence" | "loiter" | "line_cross" => {}
            other => {
                return Err(format!(
                    "{ctx}: mode must be \"presence\", \"line_cross\", or \"loiter\" (got \"{other}\")"
                ));
            }
        }
        if rule
            .zone
            .iter()
            .any(|[x, y]| !(0.0..=1.0).contains(x) || !(0.0..=1.0).contains(y))
        {
            return Err(format!("{ctx}: zone points must be within 0.0..=1.0"));
        }
        if rule.mode == "line_cross"
            && !matches!(rule.direction.as_str(), "a_to_b" | "b_to_a" | "either")
        {
            return Err(format!(
                "{ctx}: direction must be \"a_to_b\", \"b_to_a\", or \"either\" (got \"{}\")",
                rule.direction
            ));
        }
        if rule.mode == "loiter" && rule.dwell_s == 0 {
            return Err(format!(
                "{ctx}: dwell_s must be non-zero for mode \"loiter\""
            ));
        }
        if !labels.is_empty() {
            if let Some(unknown) = rule.classes.iter().find(|c| !labels.contains(c)) {
                return Err(format!("{ctx}: class \"{unknown}\" is not in ai.labels"));
            }
        }
        for action in &rule.actions {
            match action.as_str() {
                "snapshot" | "clip" | "cluster_broadcast" => {}
                "webhook" if rule.webhook_url.is_empty() => {
                    return Err(format!("{ctx}: action \"webhook\" needs webhook_url set"));
                }
                "gpio_output" if rule.gpio_chip.is_empty() => {
                    return Err(format!("{ctx}: action \"gpio_output\" needs gpio_chip set"));
                }
                "webhook" | "gpio_output" => {}
                other => {
                    return Err(format!(
                        "{ctx}: unknown action \"{other}\" (built-in: snapshot, clip, \
                         cluster_broadcast, webhook, gpio_output)"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Reject configurations that would boot into a broken state. Errors name the
/// offending key so a field technician can fix the file without reading code.
fn validate(cfg: &AppConfig) -> Result<(), Box<dyn std::error::Error>> {
    let codec = cfg.stream.codec.to_lowercase();
    if !matches!(codec.as_str(), "h264" | "avc" | "h265" | "hevc") {
        return Err(format!(
            "stream.codec must be \"h264\" or \"h265\" (got \"{}\")",
            cfg.stream.codec
        )
        .into());
    }
    if cfg.camera.width == 0 || cfg.camera.height == 0 {
        return Err("camera.width/camera.height must be non-zero".into());
    }
    if cfg.camera.fps == 0 {
        return Err("camera.fps must be non-zero".into());
    }
    if !cfg.stream.rtsp_path.starts_with('/') {
        return Err(format!(
            "stream.rtsp_path must start with '/' (got \"{}\")",
            cfg.stream.rtsp_path
        )
        .into());
    }
    if cfg.system.queue_capacity == 0 {
        return Err("system.queue_capacity must be non-zero".into());
    }
    // A FrameHandle is a zero-copy pointer into a fixed-size userspace pool
    // (crate::hal::FRAME_POOL_SIZE) recycled by capture rate alone, not by
    // whether anything still references the slot. If a frame can sit queued
    // (up to queue_capacity deep, on either the AI or stream queue) for as
    // long as it takes FRAME_POOL_SIZE more frames to be captured, the
    // capture thread can overwrite that slot's memory while it's still being
    // read — silently, no crash, no log, just torn frame data. Requiring
    // real margin here catches a misconfiguration at boot instead of in the
    // field as unexplained visual/detection glitches.
    if cfg.system.queue_capacity + 2 > crate::hal::FRAME_POOL_SIZE {
        return Err(format!(
            "system.queue_capacity ({}) leaves too little margin under the {}-slot frame pool \
             (crate::hal::FRAME_POOL_SIZE) — a frame could be overwritten while still queued. \
             Lower queue_capacity to at most {}, or raise FRAME_POOL_SIZE in src/hal/mod.rs.",
            cfg.system.queue_capacity,
            crate::hal::FRAME_POOL_SIZE,
            crate::hal::FRAME_POOL_SIZE.saturating_sub(2),
        )
        .into());
    }
    if !(cfg.ai.roi.is_empty() || cfg.ai.roi.len() == 4) {
        return Err(format!(
            "ai.roi must be [] or [x, y, w, h] (got {} values)",
            cfg.ai.roi.len()
        )
        .into());
    }
    if !(0.0..=1.0).contains(&cfg.ai.confidence_threshold) {
        return Err("ai.confidence_threshold must be within 0.0..=1.0".into());
    }
    if !(0.0..=1.0).contains(&cfg.ai.nms_iou_threshold) {
        return Err("ai.nms_iou_threshold must be within 0.0..=1.0".into());
    }
    validate_ai_rules(&cfg.ai.rules, &cfg.ai.labels)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    if cfg.ptz.enabled {
        if cfg.ptz.serial_device.is_empty() {
            return Err("ptz.serial_device must be set when ptz.enabled is true".into());
        }
        if cfg.ptz.driver != "pelco_d" {
            return Err(format!(
                "ptz.driver \"{}\" is not a registered PTZ backend (built-in: pelco_d)",
                cfg.ptz.driver
            )
            .into());
        }
    }
    for bridge in &cfg.mqtt_bridge {
        let ctx = format!("mqtt_bridge[\"{}\"]", bridge.name);
        if bridge.name.is_empty() {
            return Err("every mqtt_bridge entry needs a non-empty name".into());
        }
        if bridge.host.is_empty() {
            return Err(format!("{ctx}: host must be set").into());
        }
        if bridge.topic_filter.is_empty() {
            return Err(format!("{ctx}: topic_filter must be set").into());
        }
    }
    if cfg.radar.enabled && cfg.radar.range_max_m <= cfg.radar.range_min_m {
        return Err(format!(
            "radar.range_max_m ({}) must be greater than radar.range_min_m ({})",
            cfg.radar.range_max_m, cfg.radar.range_min_m
        )
        .into());
    }
    validate_radar_zones(&cfg.radar.zones)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    Ok(())
}

/// Validates `[[radar.zones]]` on its own — same reason `validate_ai_rules`
/// is split out: a future remote radar-config endpoint (mirroring Phase
/// 20's `config_set_ai_rules`) could reuse this without duplicating the
/// checks.
pub fn validate_radar_zones(zones: &[RadarZone]) -> Result<(), String> {
    for zone in zones {
        let ctx = format!("radar.zones[\"{}\"]", zone.name);
        if zone.name.is_empty() {
            return Err("every radar.zones entry needs a non-empty name".to_string());
        }
        match zone.mode.as_str() {
            "presence" | "loiter" if zone.points.len() < 3 => {
                return Err(format!(
                    "{ctx}: mode \"{}\" needs a polygon (>=3 points), got {}",
                    zone.mode,
                    zone.points.len()
                ));
            }
            "line_cross" if zone.points.len() != 2 => {
                return Err(format!(
                    "{ctx}: mode \"line_cross\" needs exactly 2 points, got {}",
                    zone.points.len()
                ));
            }
            "presence" | "loiter" | "line_cross" => {}
            other => {
                return Err(format!(
                    "{ctx}: mode must be \"presence\", \"line_cross\", or \"loiter\" (got \"{other}\")"
                ));
            }
        }
        if zone.mode == "line_cross"
            && !matches!(zone.direction.as_str(), "a_to_b" | "b_to_a" | "either")
        {
            return Err(format!(
                "{ctx}: direction must be \"a_to_b\", \"b_to_a\", or \"either\" (got \"{}\")",
                zone.direction
            ));
        }
        if zone.mode == "loiter" && zone.dwell_s == 0 {
            return Err(format!(
                "{ctx}: dwell_s must be non-zero for mode \"loiter\""
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::load_config;

    /// Every shipped `config/presets/*.toml` must be a real, bootable
    /// config — same parse+validate path main.rs uses for
    /// config/iiotedge_default.toml — not just an illustrative snippet
    /// that happens to look right. Guards against the presets silently
    /// bit-rotting as the config schema evolves out from under them.
    #[test]
    fn every_shipped_preset_parses_and_validates() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/presets");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("config/presets must exist") {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            load_config(&path)
                .unwrap_or_else(|e| panic!("preset {} failed to load: {e}", path.display()));
            checked += 1;
        }
        assert!(
            checked >= 8,
            "expected at least 8 preset files in {}, found {checked}",
            dir.display()
        );
    }
}
