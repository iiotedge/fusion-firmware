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
    /// `[[tags]]`: southbound machine/bridge events -> named signals (src/tags.rs).
    #[serde(default)]
    pub tags: Vec<TagConfig>,
    /// `[signals]`: the named-signal layer (src/signals.rs).
    #[serde(default)]
    pub signals: SignalsConfig,
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

/// `[signals]`: knobs of the named-signal layer (src/signals.rs).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct SignalsConfig {
    /// What a `push:<name>` signal reads from the moment the firmware starts until
    /// something pushes it (`POST /signals/<name>`): `name = true | false | <number>`.
    ///
    /// For bench and virtual devices. A pushed value lives in memory, so after every
    /// restart a signal nobody has pushed yet reads "no reading", and a controller shows
    /// the endpoint reading it as "No Response" - which is the honest answer for a real
    /// sensor, but only a nuisance for a demo node that is restarted all day. Naming a
    /// signal here is the operator saying "this one reads X until told otherwise", so
    /// leave real southbound feeds out (a `[[tags]]` signal is refused here: its value
    /// comes from its source, and expires when the source goes quiet).
    #[serde(default)]
    pub initial: std::collections::BTreeMap<String, SignalInitial>,
}

/// One initial signal value: a boolean or a number.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq)]
#[serde(untagged)]
pub enum SignalInitial {
    Bool(bool),
    Number(f64),
}

/// Validates `[signals]`: usable names, finite numbers, and nothing a `[[tags]]` entry feeds.
pub fn validate_signals(cfg: &SignalsConfig, tags: &[TagConfig]) -> Result<(), String> {
    for (name, value) in &cfg.initial {
        if !crate::signals::valid_name(name) {
            return Err(format!(
                "signals.initial.{name}: a signal name is 1-64 characters of letters, digits, '_', '-' or '.'"
            ));
        }
        if let SignalInitial::Number(n) = value {
            if !n.is_finite() {
                return Err(format!("signals.initial.{name} must be a finite number"));
            }
        }
        if tags.iter().any(|t| &t.signal == name) {
            return Err(format!(
                "signals.initial.{name} is fed by a [[tags]] entry: its value comes from that source, not from an initial one"
            ));
        }
    }
    Ok(())
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
    /// Where the Device Attestation material comes from: `"test"` (default) or
    /// `"files"`.
    ///
    /// `"test"` is rs-matter's own Matter TEST credentials - the ones `chip-tool`
    /// expects out of the box: vendor id 0xFFF1, product id 0x8001, a test DAC/PAI
    /// and a test Certification Declaration. Any controller accepts them for
    /// development, and Apple Home shows its "has not been certified" notice,
    /// because nothing chains to a CSA-approved root.
    ///
    /// `"files"` loads the real thing from `dac_file`, `dac_key_file`, `pai_file`
    /// and `cd_file`, with `vendor_id`/`product_id` set to the ids those
    /// certificates carry. The firmware checks at start-up (and in
    /// `--check-config`) that the key belongs to the DAC, the DAC was issued by the
    /// PAI, and the vendor/product ids in the DAC, the PAI and the Certification
    /// Declaration agree with the configured ones, so a wrong file is reported
    /// plainly instead of surfacing as a controller's "unable to add".
    #[serde(default = "default_matter_attestation")]
    pub attestation: String,
    /// Matter vendor id (`VendorID`, 1..=0xFFFF). Default 0xFFF1, the Matter TEST
    /// vendor, which is the only value that works with `attestation = "test"`: a
    /// controller refuses a node whose DAC carries a different vendor id than the
    /// one it reports.
    #[serde(default = "default_matter_vendor_id")]
    pub vendor_id: u16,
    /// Matter product id (`ProductID`). Default 0x8001 (the test DAC's); with
    /// `attestation = "files"` it must be the product id in the DAC.
    #[serde(default = "default_matter_product_id")]
    pub product_id: u16,
    /// The setup passcode a controller needs to add this node (the number inside
    /// the QR code and the manual pairing code), 1..=99999998. Default 20202021,
    /// the PUBLIC test code every build shares: anyone on the LAN who knows it can
    /// add the node while its pairing window is open, so give each device its own
    /// when it leaves the bench.
    #[serde(default = "default_matter_setup_passcode")]
    pub setup_passcode: u32,
    /// The 12-bit discriminator advertised while the node can be added (0..=4095).
    /// Default 3840.
    #[serde(default = "default_matter_discriminator")]
    pub discriminator: u16,
    /// `attestation = "files"`: the Device Attestation Certificate, DER.
    #[serde(default)]
    pub dac_file: String,
    /// `attestation = "files"`: the DAC's private key - PEM or DER, SEC1 ("EC
    /// PRIVATE KEY") or PKCS#8 ("PRIVATE KEY"), or the raw 32-byte scalar. Keep it
    /// readable by the service user only (0600): the firmware warns otherwise.
    #[serde(default)]
    pub dac_key_file: String,
    /// `attestation = "files"`: the Product Attestation Intermediate that issued the DAC, DER.
    #[serde(default)]
    pub pai_file: String,
    /// `attestation = "files"`: the Certification Declaration (a CMS signed by the CSA), DER.
    #[serde(default)]
    pub cd_file: String,
    /// This device's Camera endpoint (WebRTC Transport Provider, Camera AV
    /// Stream Management, Zone Management — src/matter/camera.rs). Its own
    /// switch, independent of `[matter].enabled`, so a deployment can turn
    /// Matter on for other endpoints (`[matter.onoff]`, …) without also
    /// exposing camera clusters — e.g. a device with no camera hardware
    /// meaningfully attached at all. Defaults to the pre-existing behavior
    /// (on) so an already-deployed `[matter]` config with no `[matter.camera]`
    /// section keeps working exactly as before.
    #[serde(default)]
    pub camera: MatterCameraConfig,
    /// A second, independent Matter endpoint: a plain On/Off Light/Switch
    /// (Matter's `OnOff` cluster, 0x0006) backed by a REAL GPIO output line
    /// — this is what makes "deploy this firmware as a light switch, not a
    /// camera" a real, config-only choice rather than a hypothetical: set
    /// `[matter].enabled = true`, `[matter.camera].enabled = false`,
    /// `[matter.onoff].enabled = true` with a real `gpio_chip`/`gpio_line`,
    /// and the Matter fabric sees a light switch with no camera clusters at
    /// all. See src/matter/onoff.rs.
    #[serde(default)]
    pub onoff: MatterOnOffConfig,
    /// A third, independent Matter endpoint (Phase 19e): a full Matter
    /// "Extended Color Light" (On/Off + LevelControl + ColorControl,
    /// hue/saturation + XY + color temperature + color loop) — this is the
    /// "light bulb with hue support" endpoint, distinct from the plain
    /// relay/switch in `[matter.onoff]` (own endpoint id, no collision, can
    /// be enabled independently or alongside it). No PWM/RGB driver exists
    /// on this board today, so state is honestly in-memory only — every
    /// attribute and command is real and spec-compliant (a controller can
    /// turn it on/off, dim it, and set its color and see the change stick),
    /// it just doesn't drive a physical light yet. See src/matter/light.rs.
    #[serde(default)]
    pub light: MatterLightConfig,
    /// A fourth, independent Matter endpoint (Phase 19f): a Thermostat
    /// (0x0201) — the first Matter cluster in this firmware with NO
    /// ready-made application handler in rs-matter 0.3.0 (currently built
    /// against its raw `Handler` trait; see src/matter/thermostat.rs's
    /// header for the full confidence/verification notes). This board has
    /// no HVAC equipment, so `SystemMode` and both setpoints are honestly
    /// in-memory only; `LocalTemperature` is the one real signal, backed
    /// by the SoC's own thermal-zone reading (device temperature, not
    /// room-ambient).
    #[serde(default)]
    pub thermostat: MatterThermostatConfig,
    /// Config-driven Matter endpoints (`[[matter.endpoints]]`, Phase 19g): any
    /// number of sensors/devices, each bound to a data source by a short spec
    /// string — see `MatterEndpointConfig` and src/signals.rs. This is what
    /// makes the firmware generic: the same binary becomes a temperature
    /// sensor, an occupancy sensor, a contact sensor, ... purely by config.
    /// Independent of the four legacy `[matter.*]` sections above, which keep
    /// working unchanged with their historical endpoint ids.
    #[serde(default)]
    pub endpoints: Vec<MatterEndpointConfig>,
    /// Who made this node, as a controller's accessory details show it (Matter
    /// BasicInformation `VendorName`, max 32 bytes). Default "IIoTEdge".
    #[serde(default)]
    pub vendor_name: String,
    /// What this node is called as a product (`ProductName`, max 32 bytes).
    /// Default: "fusion-firmware Camera" when `[matter.camera]` is on (unchanged
    /// for already-paired cameras), else "fusion-firmware" — a light switch
    /// should not introduce itself as a camera. Set it per product.
    #[serde(default)]
    pub product_name: String,
    /// The name in the commissioning advertisement (mDNS `DN`, max 32 bytes).
    /// Default "fusion-firmware <device_id>".
    #[serde(default)]
    pub device_name: String,
}

fn default_matter_state_dir() -> String {
    "config/matter_state".to_string()
}

fn default_matter_attestation() -> String {
    "test".to_string()
}

/// The Matter TEST vendor id, product id, passcode and discriminator: what every
/// build has always used (rs-matter's `TEST_DEV_*`, what `chip-tool` expects).
pub const MATTER_TEST_VENDOR_ID: u16 = 0xFFF1;
pub const MATTER_TEST_PRODUCT_ID: u16 = 0x8001;
pub const MATTER_TEST_PASSCODE: u32 = 20_202_021;
pub const MATTER_TEST_DISCRIMINATOR: u16 = 3840;

fn default_matter_vendor_id() -> u16 {
    MATTER_TEST_VENDOR_ID
}
fn default_matter_product_id() -> u16 {
    MATTER_TEST_PRODUCT_ID
}
fn default_matter_setup_passcode() -> u32 {
    MATTER_TEST_PASSCODE
}
fn default_matter_discriminator() -> u16 {
    MATTER_TEST_DISCRIMINATOR
}

impl Default for MatterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            state_dir: default_matter_state_dir(),
            attestation: default_matter_attestation(),
            vendor_id: default_matter_vendor_id(),
            product_id: default_matter_product_id(),
            setup_passcode: default_matter_setup_passcode(),
            discriminator: default_matter_discriminator(),
            dac_file: String::new(),
            dac_key_file: String::new(),
            pai_file: String::new(),
            cd_file: String::new(),
            camera: MatterCameraConfig::default(),
            onoff: MatterOnOffConfig::default(),
            light: MatterLightConfig::default(),
            thermostat: MatterThermostatConfig::default(),
            endpoints: Vec::new(),
            vendor_name: String::new(),
            product_name: String::new(),
            device_name: String::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MatterCameraConfig {
    pub enabled: bool,
}

impl Default for MatterCameraConfig {
    fn default() -> Self {
        // Preserves this feature's original behavior (camera-only Matter
        // support, no separate toggle) for any config written before this
        // field existed.
        Self { enabled: true }
    }
}

/// Backing for a real Matter OnOff (0x0006) cluster — a plain relay/light
/// switch, no dimming (Matter's LevelControl cluster) or color (ColorControl)
/// hardware assumed. `gpio_chip`/`gpio_line` follow the exact same
/// `gpio-cdev` convention `[[ai.rules]]`'s `gpio_output` action already uses
/// (see src/ai/actions.rs), so an existing GPIO wiring can be reused as
/// either an AI-rule-triggered pulse or a persistent Matter on/off switch —
/// just not both on the SAME line at once (the two would fight over the
/// line's open handle).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MatterOnOffConfig {
    pub enabled: bool,
    #[serde(default)]
    pub gpio_chip: String,
    #[serde(default)]
    pub gpio_line: u32,
    /// Some relay boards wire the "on" logic level inverted (driving the
    /// GPIO low turns the relay on). false = active-high (GPIO high = on),
    /// the common case for a direct transistor/MOSFET-driven load.
    #[serde(default)]
    pub active_low: bool,
}

/// Backing for the `[matter.light]` endpoint (src/matter/light.rs) — a
/// full color-capable Matter light, independent of `[matter.onoff]`'s plain
/// relay/switch. No hardware fields yet (no PWM/RGB driver exists on this
/// board): this is intentionally the same "real cluster and protocol
/// behavior now, real GPIO/PWM the moment hardware is wired" shape
/// `MatterOnOffConfig` used before it gained `gpio_chip`/`gpio_line` —
/// future PWM/RGB pin fields belong here once real hardware exists.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MatterLightConfig {
    pub enabled: bool,
}

/// Backing for the `[matter.thermostat]` endpoint (src/matter/thermostat.rs).
/// No hardware fields — there is no real HVAC equipment on this board to
/// configure a relay/contactor for yet; this is a reporting+virtual-control
/// shell, not a real climate controller.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct MatterThermostatConfig {
    pub enabled: bool,
}

/// What a `[[matter.endpoints]]` entry becomes on the Matter fabric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatterEndpointKind {
    // Sensors: read a `source`.
    Temperature,
    Humidity,
    Pressure,
    Flow,
    Illuminance,
    Occupancy,
    Contact,
    // More boolean "detected" sensors (true = detected): they share the Contact
    // sensor's BooleanState cluster and differ only in device type.
    WaterLeak,
    Rain,
    WaterFreeze,
    // Matter 1.5 soil sensor: a percentage (0-100).
    SoilMoisture,
    // Air Quality Sensor: ONE endpoint with several measurements (CO2, PM2.5,
    // TVOC, ...) — takes a `sources` table instead of a single `source`.
    AirQuality,
    // Actuators: drive a `sink`.
    OnOffLight,
    OnOffPlug,
    Fan,
    // A push button or toggle: reads a boolean `source` and emits press events.
    GenericSwitch,
}

impl MatterEndpointKind {
    pub const ALL: [(&'static str, MatterEndpointKind); 16] = [
        ("temperature_sensor", Self::Temperature),
        ("humidity_sensor", Self::Humidity),
        ("pressure_sensor", Self::Pressure),
        ("flow_sensor", Self::Flow),
        ("illuminance_sensor", Self::Illuminance),
        ("occupancy_sensor", Self::Occupancy),
        ("contact_sensor", Self::Contact),
        ("water_leak_sensor", Self::WaterLeak),
        ("rain_sensor", Self::Rain),
        ("water_freeze_sensor", Self::WaterFreeze),
        ("soil_moisture_sensor", Self::SoilMoisture),
        ("air_quality_sensor", Self::AirQuality),
        ("on_off_light", Self::OnOffLight),
        ("on_off_plug", Self::OnOffPlug),
        ("fan", Self::Fan),
        ("generic_switch", Self::GenericSwitch),
    ];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|(n, _)| *n == s).map(|(_, k)| *k)
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .map(|(n, _)| *n)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The Matter device type id and revision this kind is exposed as — defined
    /// once here so the endpoint builders and the node's advertised device type
    /// can't drift apart.
    pub const fn device_type(self) -> (u16, u16) {
        match self {
            Self::Temperature => (0x0302, 3),
            Self::Humidity => (0x0307, 3),
            Self::Pressure => (0x0305, 3),
            Self::Flow => (0x0306, 3),
            Self::Illuminance => (0x0106, 4),
            Self::Occupancy => (0x0107, 4),
            Self::Contact => (0x0015, 2),
            Self::WaterLeak => (0x0043, 2),
            Self::Rain => (0x0044, 2),
            Self::WaterFreeze => (0x0041, 2),
            Self::SoilMoisture => (0x0045, 1),
            Self::AirQuality => (0x002C, 1),
            Self::OnOffLight => (0x0100, 3),
            Self::OnOffPlug => (0x010A, 4),
            Self::Fan => (0x002B, 4),
            Self::GenericSwitch => (0x000F, 3),
        }
    }

    /// Actuators are driven through a `sink`; sensors are read from a `source`.
    pub fn is_actuator(self) -> bool {
        matches!(self, Self::OnOffLight | Self::OnOffPlug | Self::Fan)
    }

    /// One endpoint with several named measurements: configured with a
    /// `sources` table, not a single `source`.
    pub fn is_multi_source(self) -> bool {
        matches!(self, Self::AirQuality)
    }

    /// Boolean sensors read a true/false signal; the other sensors read a number.
    pub fn is_boolean(self) -> bool {
        matches!(
            self,
            Self::Occupancy
                | Self::Contact
                | Self::WaterLeak
                | Self::Rain
                | Self::WaterFreeze
                | Self::GenericSwitch
        )
    }
}

fn default_one() -> f64 {
    1.0
}

/// Sensors and actuators notice changes once a second by default; a switch has
/// to catch a short press, so it samples much faster.
pub const DEFAULT_POLL_MS: u64 = 1000;
pub const SWITCH_DEFAULT_POLL_MS: u64 = 20;
pub const SWITCH_DEFAULT_LONG_PRESS_MS: u64 = 800;
pub const SWITCH_DEFAULT_MULTI_PRESS_MS: u64 = 300;
pub const SWITCH_DEFAULT_MULTI_PRESS_MAX: u8 = 3;
pub const SWITCH_DEFAULT_DEBOUNCE_MS: u64 = 30;

/// One `[[matter.endpoints]]` entry: a Matter device whose value comes from a
/// `source` (see src/signals.rs for the spec grammar). Example:
///
/// ```toml
/// [[matter.endpoints]]
/// kind   = "temperature_sensor"
/// name   = "Board temperature"
/// source = "builtin:soc_temp_c"
/// ```
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MatterEndpointConfig {
    /// One of `MatterEndpointKind::names()`.
    pub kind: String,
    /// Shown to integrators and used as the endpoint's stable UniqueID (max 32
    /// bytes). Defaults to `<kind>_<position>`; must be unique.
    #[serde(default)]
    pub name: String,
    /// Pin a specific endpoint id (>= 5; 1-4 belong to the legacy sections).
    /// Without it ids are assigned in config order from 16 — pin ids in
    /// production so reordering/removing an entry can't renumber the rest.
    #[serde(default)]
    pub endpoint: Option<u16>,
    /// SENSOR kinds: where the value comes from — `builtin:<name>`,
    /// `sysfs:<path>`, `gpio_in:<chip>:<line>[:active_low]` or `push:<name>`.
    #[serde(default)]
    pub source: String,
    /// ACTUATOR kinds (on_off_light, on_off_plug, fan): where the command goes —
    /// `gpio:<chip>:<line>[:active_low]` (a relay/MOSFET line, driven off at
    /// boot), `signal:<name>` (publish the commanded state to the signal bus /
    /// `GET /signals`) or `virtual` (no hardware; must be asked for explicitly —
    /// an actuator with no sink is a config error, not a silent no-op).
    #[serde(default)]
    pub sink: String,
    /// Fan only: the speeds the fan REALLY has — `off_high` (default: one speed,
    /// a plain relay), `off_low_high` or `off_low_med_high`. A `gpio:` sink is a
    /// single on/off line, so it can only be `off_high`; a multi-speed fan needs
    /// a `signal:` sink that something else turns into real speeds.
    #[serde(default)]
    pub fan_speeds: String,
    /// Numeric kinds: reading = source * scale + offset, in the kind's natural
    /// unit (C for temperature, % for humidity and soil moisture, hPa for
    /// pressure, lux for illuminance, m3/h for flow). E.g. a sysfs file in
    /// millidegrees: scale 0.001.
    #[serde(default = "default_one")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
    /// Numeric kinds: the sensor's physical range, in the natural unit. A
    /// reading outside it is reported as "no reading" rather than trusted.
    /// Defaults are per kind.
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// Boolean kinds: report the opposite of the source (e.g. a normally-closed
    /// reed switch). Contact sensor: true = closed; leak / rain / freeze
    /// sensors: true = detected.
    #[serde(default)]
    pub invert: bool,
    /// Occupancy sensors: the sensing technology the endpoint claims — "pir"
    /// (default), "ultrasonic", "physical_contact", or (Matter 1.5) "vision" /
    /// "radar" / "other". Use "vision" for camera-AI-derived presence and
    /// "radar" for a radar zone.
    #[serde(default)]
    pub occupancy_type: String,
    /// Multi-source kinds (air_quality_sensor): one named source per measurement,
    /// each a source spec like `source`: `co2`, `co`, `no2`, `o3`, `pm1`, `pm25`,
    /// `pm10`, `tvoc`, `formaldehyde`, `radon`, `temperature`, `humidity`, and
    /// `air_quality` (a 0..=6 level the device already computes, which then wins
    /// over the level derived from the concentrations). At least one pollutant or
    /// `air_quality` is required. Example:
    /// `sources = { co2 = "push:co2", pm25 = "sysfs:/sys/bus/iio/devices/iio:device0/in_massconcentration_pm2p5_input" }`.
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, String>,
    /// Multi-source kinds: per-source scale (reading = source * scale) to bring a
    /// source into the Matter unit — ppm for CO2 and CO, ppb for NO2, ozone, TVOC
    /// and formaldehyde, ug/m3 for PM, Bq/m3 for radon, C and % for temperature
    /// and humidity.
    #[serde(default)]
    pub scales: std::collections::BTreeMap<String, f64>,
    /// Occupancy sensors: keep reporting occupied for this long (ms, 0..=3600000)
    /// after the last true reading. A camera-AI detection or a rule match is a
    /// momentary pulse (a `presence` rule re-fires on every inference frame while
    /// someone is in the zone), and a PIR can flicker, so without a hold an
    /// occupancy sensor would flap. Applied after `invert`.
    #[serde(default)]
    pub hold_ms: Option<u64>,
    /// Generic switch only: `momentary` (default; a push button — InitialPress /
    /// ShortRelease / LongPress / multi-press events) or `latching` (a toggle or
    /// rocker — a SwitchLatched event on every change). The source is true while
    /// the button is pressed / the switch is in its second position.
    #[serde(default)]
    pub switch_mode: String,
    /// Momentary switch: how long a press is held to count as a long press, in
    /// ms (200..=10000; default 800).
    #[serde(default)]
    pub long_press_ms: Option<u64>,
    /// Momentary switch: how soon after a release another press still belongs to
    /// the same multi-press sequence, in ms (100..=1000; default 300).
    #[serde(default)]
    pub multi_press_ms: Option<u64>,
    /// Momentary switch: the most consecutive presses counted (2..=20; default 3).
    #[serde(default)]
    pub multi_press_max: Option<u8>,
    /// Generic switch: ignore changes shorter than this (contact bounce), in ms
    /// (0..=500; default 30; 0 = none).
    #[serde(default)]
    pub debounce_ms: Option<u64>,
    /// How often the source is sampled to notice changes, in ms. Default 1000
    /// (100..=3600000); 20 for a generic_switch (5..=1000), which has to catch a
    /// short press.
    #[serde(default)]
    pub poll_ms: Option<u64>,
    /// Allow a source marked synthetic (the mock camera/radar). Off by default
    /// so a real controller is never shown fake readings; turn on for bench/dev.
    #[serde(default)]
    pub allow_mock: bool,
}

/// Validates the identity strings (`vendor_name`, `product_name`, `device_name`):
/// the BasicInformation attributes are length-limited and a controller shows
/// them verbatim.
pub fn validate_matter_identity(cfg: &MatterConfig) -> Result<(), String> {
    for (field, value) in [
        ("vendor_name", &cfg.vendor_name),
        ("product_name", &cfg.product_name),
        ("device_name", &cfg.device_name),
    ] {
        if value.len() > 32 || value.chars().any(char::is_control) {
            return Err(format!(
                "matter.{field} '{value}' must be at most 32 bytes with no control characters"
            ));
        }
    }
    Ok(())
}

/// The passcodes the Matter spec forbids (trivially guessable), besides 0.
const MATTER_INVALID_PASSCODES: [u32; 11] = [
    11_111_111, 22_222_222, 33_333_333, 44_444_444, 55_555_555, 66_666_666, 77_777_777, 88_888_888,
    99_999_999, 12_345_678, 87_654_321,
];

/// Validates the vendor/product ids, the setup passcode and discriminator and the
/// attestation settings. Pure checks - no file is opened here (the loader in
/// `matter::attestation` reads and verifies the files at boot and in
/// `--check-config`).
pub fn validate_matter_attestation(cfg: &MatterConfig) -> Result<(), String> {
    if cfg.vendor_id == 0 {
        return Err("matter.vendor_id must not be 0".to_string());
    }
    if cfg.setup_passcode == 0
        || cfg.setup_passcode > 99_999_998
        || MATTER_INVALID_PASSCODES.contains(&cfg.setup_passcode)
    {
        return Err(format!(
            "matter.setup_passcode {} is not a valid Matter passcode: 1..=99999998, and not one of 11111111, 22222222, ..., 99999999, 12345678, 87654321",
            cfg.setup_passcode
        ));
    }
    if cfg.discriminator > 0x0FFF {
        return Err(format!(
            "matter.discriminator {} must fit in 12 bits (0..=4095)",
            cfg.discriminator
        ));
    }
    match cfg.attestation.as_str() {
        "test" => {
            if (cfg.vendor_id, cfg.product_id) != (MATTER_TEST_VENDOR_ID, MATTER_TEST_PRODUCT_ID) {
                return Err(format!(
                    "matter.vendor_id 0x{:04X} / matter.product_id 0x{:04X} need attestation = \"files\": the built-in test certificates are for vendor 0x{MATTER_TEST_VENDOR_ID:04X} product 0x{MATTER_TEST_PRODUCT_ID:04X} only, and a controller refuses a node whose certificate names a different vendor or product than the one it reports",
                    cfg.vendor_id, cfg.product_id
                ));
            }
            for (key, value) in [
                ("dac_file", &cfg.dac_file),
                ("dac_key_file", &cfg.dac_key_file),
                ("pai_file", &cfg.pai_file),
                ("cd_file", &cfg.cd_file),
            ] {
                if !value.is_empty() {
                    return Err(format!("matter.{key} is set but matter.attestation = \"test\": set attestation = \"files\" to use it"));
                }
            }
        }
        "files" => {
            for (key, value) in [
                ("dac_file", &cfg.dac_file),
                ("dac_key_file", &cfg.dac_key_file),
                ("pai_file", &cfg.pai_file),
                ("cd_file", &cfg.cd_file),
            ] {
                if value.trim().is_empty() {
                    return Err(format!("matter.attestation = \"files\" needs matter.{key}"));
                }
            }
        }
        other => {
            return Err(format!(
                "matter.attestation '{other}' must be \"test\" or \"files\""
            ))
        }
    }
    Ok(())
}

const MAX_MATTER_ENDPOINTS: usize = 64;
const MATTER_RESERVED_ENDPOINT_IDS: std::ops::RangeInclusive<u16> = 1..=4;

/// Validates `[[matter.endpoints]]`. Pure syntax/consistency checks — nothing
/// is opened here (no GPIO, no files), so a typo is caught at boot without
/// touching hardware.
pub fn validate_matter_endpoints(endpoints: &[MatterEndpointConfig]) -> Result<(), String> {
    if endpoints.len() > MAX_MATTER_ENDPOINTS {
        return Err(format!(
            "matter.endpoints: {} entries exceeds the {MAX_MATTER_ENDPOINTS}-endpoint limit",
            endpoints.len()
        ));
    }
    let mut names = std::collections::HashSet::new();
    let mut pinned = std::collections::HashSet::new();
    for (i, e) in endpoints.iter().enumerate() {
        let at = format!("matter.endpoints[{i}]");
        let kind = MatterEndpointKind::parse(&e.kind).ok_or_else(|| {
            format!(
                "{at}: unknown kind '{}' (available: {})",
                e.kind,
                MatterEndpointKind::names()
            )
        })?;
        let name = effective_endpoint_name(e, i);
        if name.len() > 32 || name.chars().any(char::is_control) {
            return Err(format!(
                "{at}: name '{name}' must be at most 32 bytes with no control characters"
            ));
        }
        if !names.insert(name.clone()) {
            return Err(format!("{at}: duplicate name '{name}'"));
        }
        if let Some(id) = e.endpoint {
            if id == 0 || MATTER_RESERVED_ENDPOINT_IDS.contains(&id) {
                return Err(format!(
                    "{at}: endpoint id {id} is reserved (0 = root, 1-4 = the legacy [matter.*] sections)"
                ));
            }
            if !pinned.insert(id) {
                return Err(format!("{at}: endpoint id {id} is pinned more than once"));
            }
        }
        if kind.is_actuator() {
            if !e.source.is_empty() {
                return Err(format!(
                    "{at} ({name}): `source` is for sensor kinds; {} is driven through a `sink`",
                    e.kind
                ));
            }
            if e.sink.is_empty() {
                return Err(format!(
                    "{at} ({name}): `sink` is required (gpio:<chip>:<line>, signal:<name> or virtual) — \
                     an actuator with no sink would silently do nothing"
                ));
            }
            let sink = crate::signals::parse_sink_spec(&e.sink)
                .map_err(|err| format!("{at} ({name}): {err}"))?;
            if kind == MatterEndpointKind::Fan {
                let steps = crate::matter::FanSteps::parse(&e.fan_speeds).ok_or_else(|| {
                    format!(
                        "{at} ({name}): fan_speeds '{}' must be one of: {}",
                        e.fan_speeds,
                        crate::matter::FanSteps::names()
                    )
                })?;
                if matches!(sink, crate::signals::SinkSpec::Gpio { .. })
                    && steps != crate::matter::FanSteps::Single
                {
                    return Err(format!(
                        "{at} ({name}): a gpio: sink is one on/off line, so the fan can only be \
                         fan_speeds = \"off_high\" (use a signal: sink for more speeds)"
                    ));
                }
            } else if !e.fan_speeds.is_empty() {
                return Err(format!(
                    "{at} ({name}): fan_speeds only applies to kind = \"fan\""
                ));
            }
        } else if kind.is_multi_source() {
            if !e.source.is_empty() {
                return Err(format!(
                    "{at} ({name}): `source` is for single-source kinds; {} takes a `sources` table",
                    e.kind
                ));
            }
            if !e.sink.is_empty() {
                return Err(format!("{at} ({name}): `sink` is for actuator kinds"));
            }
            if !e.fan_speeds.is_empty() {
                return Err(format!(
                    "{at} ({name}): fan_speeds only applies to kind = \"fan\""
                ));
            }
            if e.sources.is_empty() {
                return Err(format!(
                    "{at} ({name}): `sources` is required (keys: {})",
                    crate::matter::air_quality::source_keys()
                ));
            }
            for (key, spec) in &e.sources {
                if !crate::matter::air_quality::is_source_key(key) {
                    return Err(format!(
                        "{at} ({name}): unknown sources key '{key}' (available: {})",
                        crate::matter::air_quality::source_keys()
                    ));
                }
                crate::signals::parse_spec(spec)
                    .map_err(|err| format!("{at} ({name}): sources.{key}: {err}"))?;
            }
            if !crate::matter::air_quality::has_air_quality_input(e.sources.keys()) {
                return Err(format!(
                    "{at} ({name}): needs at least one pollutant (co2, co, no2, o3, pm1, pm25, pm10, tvoc, \
                     formaldehyde, radon) or an `air_quality` level — otherwise it would read Unknown forever"
                ));
            }
            for (key, scale) in &e.scales {
                if !e.sources.contains_key(key) {
                    return Err(format!(
                        "{at} ({name}): scales.{key} has no matching entry in `sources`"
                    ));
                }
                if !scale.is_finite() || *scale == 0.0 {
                    return Err(format!(
                        "{at} ({name}): scales.{key} must be finite and non-zero"
                    ));
                }
            }
        } else {
            if !e.fan_speeds.is_empty() {
                return Err(format!(
                    "{at} ({name}): fan_speeds only applies to kind = \"fan\""
                ));
            }
            if !e.sink.is_empty() {
                return Err(format!(
                    "{at} ({name}): `sink` is for actuator kinds; {} reads a `source`",
                    e.kind
                ));
            }
            if e.source.is_empty() {
                return Err(format!("{at} ({name}): source is required"));
            }
            crate::signals::parse_spec(&e.source).map_err(|err| format!("{at} ({name}): {err}"))?;
        }
        let poll_range = if kind == MatterEndpointKind::GenericSwitch {
            5..=1_000
        } else {
            100..=3_600_000
        };
        if !poll_range.contains(&effective_poll_ms(kind, e)) {
            return Err(format!(
                "{at} ({name}): poll_ms must be within {}..={} for {}",
                poll_range.start(),
                poll_range.end(),
                e.kind
            ));
        }
        validate_switch_options(&at, &name, kind, e)?;
        if let Some(hold) = e.hold_ms {
            if kind != MatterEndpointKind::Occupancy {
                return Err(format!(
                    "{at} ({name}): hold_ms only applies to kind = \"occupancy_sensor\""
                ));
            }
            if hold > 3_600_000 {
                return Err(format!("{at} ({name}): hold_ms must be within 0..=3600000"));
            }
        }
        if !kind.is_multi_source() && (!e.sources.is_empty() || !e.scales.is_empty()) {
            return Err(format!(
                "{at} ({name}): sources / scales only apply to a multi-source kind (air_quality_sensor); \
                 {} takes a single `source`",
                e.kind
            ));
        }
        if !kind.is_boolean() && !kind.is_actuator() && !kind.is_multi_source() {
            if !e.scale.is_finite() || e.scale == 0.0 || !e.offset.is_finite() {
                return Err(format!(
                    "{at} ({name}): scale must be finite and non-zero, offset finite"
                ));
            }
            if let (Some(min), Some(max)) = (e.min, e.max) {
                if !min.is_finite() || !max.is_finite() || min >= max {
                    return Err(format!(
                        "{at} ({name}): min and max must be finite, with min below max"
                    ));
                }
            }
        }
        if kind == MatterEndpointKind::Occupancy
            && crate::matter::OccupancyTech::parse(&e.occupancy_type).is_none()
        {
            return Err(format!(
                "{at} ({name}): occupancy_type must be pir, ultrasonic, physical_contact, vision, radar or other"
            ));
        }
    }
    Ok(())
}

/// How often an endpoint's source is sampled: its `poll_ms`, else the kind's default.
pub fn effective_poll_ms(kind: MatterEndpointKind, e: &MatterEndpointConfig) -> u64 {
    e.poll_ms
        .unwrap_or(if kind == MatterEndpointKind::GenericSwitch {
            SWITCH_DEFAULT_POLL_MS
        } else {
            DEFAULT_POLL_MS
        })
}

/// The generic-switch options: only valid on that kind, within sane ranges, and
/// the momentary-only ones rejected on a latching switch (they'd silently do
/// nothing there).
fn validate_switch_options(
    at: &str,
    name: &str,
    kind: MatterEndpointKind,
    e: &MatterEndpointConfig,
) -> Result<(), String> {
    let momentary_only =
        e.long_press_ms.is_some() || e.multi_press_ms.is_some() || e.multi_press_max.is_some();
    if kind != MatterEndpointKind::GenericSwitch {
        if !e.switch_mode.is_empty() || momentary_only || e.debounce_ms.is_some() {
            return Err(format!(
                "{at} ({name}): switch_mode / long_press_ms / multi_press_ms / multi_press_max / \
                 debounce_ms only apply to kind = \"generic_switch\""
            ));
        }
        return Ok(());
    }
    let mode = crate::matter::SwitchMode::parse(&e.switch_mode).ok_or_else(|| {
        format!(
            "{at} ({name}): switch_mode '{}' must be one of: {}",
            e.switch_mode,
            crate::matter::SwitchMode::names()
        )
    })?;
    if mode == crate::matter::SwitchMode::Latching && momentary_only {
        return Err(format!(
            "{at} ({name}): long_press_ms / multi_press_ms / multi_press_max only apply to a momentary switch"
        ));
    }
    let in_range = |v: Option<u64>, lo: u64, hi: u64| v.is_none_or(|v| (lo..=hi).contains(&v));
    if !in_range(e.long_press_ms, 200, 10_000) {
        return Err(format!(
            "{at} ({name}): long_press_ms must be within 200..=10000"
        ));
    }
    if !in_range(e.multi_press_ms, 100, 1_000) {
        return Err(format!(
            "{at} ({name}): multi_press_ms must be within 100..=1000"
        ));
    }
    if !in_range(e.multi_press_max.map(u64::from), 2, 20) {
        return Err(format!(
            "{at} ({name}): multi_press_max must be within 2..=20"
        ));
    }
    if !in_range(e.debounce_ms, 0, 500) {
        return Err(format!("{at} ({name}): debounce_ms must be within 0..=500"));
    }
    Ok(())
}

/// The name an endpoint goes by: its configured `name`, else `<kind>_<position>`.
pub fn effective_endpoint_name(e: &MatterEndpointConfig, index: usize) -> String {
    if e.name.is_empty() {
        format!("{}_{}", e.kind, index + 1)
    } else {
        e.name.clone()
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

/// One `[[tags]]` entry: decodes a southbound machine event (a Modbus register
/// read, a serial line, an MQTT-bridge JSON message) into a named signal that
/// `[[matter.endpoints]]` — and anything else on the signal bus — reads as
/// `push:<signal>`. This is what lets a Modbus power meter or a Zigbee sensor
/// become a Matter device with no glue script. See src/tags.rs.
///
/// ```toml
/// [[tags]]
/// signal = "boiler_temp"
/// source = "modbus/plc1/boiler"   # the event's source id, exactly
/// type   = "i16"                  # u16 i16 u32 i32 f32 bool text json
/// scale  = 0.1                    # value = raw * scale + bias
/// ```
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TagConfig {
    /// The signal this feeds; read it as `push:<signal>` (letters, digits, `_ - .`).
    pub signal: String,
    /// The `source_id` of the events to decode — `modbus/<instance>/<read>`,
    /// `serial/<name>`, an MQTT topic for `[[mqtt_bridge]]`, ... — matched exactly.
    pub source: String,
    /// How to read the payload: `u16`, `i16`, `u32`, `i32`, `f32` (big-endian
    /// Modbus registers, 2 bytes each), `bool` (one byte, non-zero = true; Modbus
    /// coils), `text` (a number or true/false/on/off as text, e.g. a serial
    /// line) or `json` (a number/bool at `field`).
    #[serde(rename = "type")]
    pub data_type: String,
    /// Byte offset of the value in the payload (a Modbus register is 2 bytes, so
    /// register N of a read is offset 2*N). Default 0.
    #[serde(default)]
    pub offset: usize,
    /// 32-bit types spanning two registers: `big` (default; high register
    /// first, "ABCD") or `swap` (low register first, "CDAB" — common on meters).
    #[serde(default)]
    pub word_order: String,
    /// `type = "json"`: the field, as a dotted path (`temperature`, `data.temp`).
    #[serde(default)]
    pub field: String,
    /// value = raw * scale + bias (numbers only).
    #[serde(default = "default_one")]
    pub scale: f64,
    #[serde(default)]
    pub bias: f64,
    /// The signal reads "no data" this long after its last update (1..=86400 s;
    /// default 60) — a dead link must not look like a sensor reading the same
    /// value forever. Set it well above the poll interval.
    #[serde(default = "default_tag_max_age_s")]
    pub max_age_s: u64,
}

fn default_tag_max_age_s() -> u64 {
    60
}

/// Validates `[[tags]]`: syntax and consistency only.
pub fn validate_tags(tags: &[TagConfig]) -> Result<(), String> {
    let mut signals = std::collections::HashSet::new();
    for (i, t) in tags.iter().enumerate() {
        let at = format!("tags[{i}] ({})", t.signal);
        if !crate::signals::valid_name(&t.signal) {
            return Err(format!(
                "tags[{i}]: signal '{}' must be 1-64 chars of letters, digits, '_', '-', '.'",
                t.signal
            ));
        }
        if !signals.insert(t.signal.as_str()) {
            return Err(format!("{at}: signal is fed by more than one tag"));
        }
        if t.source.is_empty() || t.source.len() > 256 || t.source.chars().any(char::is_control) {
            return Err(format!(
                "{at}: source must be 1-256 chars with no control characters"
            ));
        }
        let data_type = crate::tags::DataType::parse(&t.data_type).ok_or_else(|| {
            format!(
                "{at}: unknown type '{}' (available: {})",
                t.data_type,
                crate::tags::DataType::names()
            )
        })?;
        if crate::tags::WordOrder::parse(&t.word_order).is_none() {
            return Err(format!("{at}: word_order must be big or swap"));
        }
        if !t.word_order.is_empty() && !data_type.is_32_bit() {
            return Err(format!(
                "{at}: word_order only applies to the 32-bit types (u32, i32, f32)"
            ));
        }
        if data_type == crate::tags::DataType::Json {
            if t.field.is_empty() {
                return Err(format!("{at}: type = \"json\" needs a `field`"));
            }
        } else if !t.field.is_empty() {
            return Err(format!("{at}: `field` only applies to type = \"json\""));
        }
        if matches!(
            data_type,
            crate::tags::DataType::Text | crate::tags::DataType::Json
        ) && t.offset != 0
        {
            return Err(format!("{at}: offset only applies to the binary types"));
        }
        if t.offset > 4096 {
            return Err(format!("{at}: offset must be at most 4096"));
        }
        if !t.scale.is_finite() || t.scale == 0.0 || !t.bias.is_finite() {
            return Err(format!(
                "{at}: scale must be finite and non-zero, bias finite"
            ));
        }
        if !(1..=86_400).contains(&t.max_age_s) {
            return Err(format!("{at}: max_age_s must be within 1..=86400"));
        }
    }
    Ok(())
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
    validate_matter_endpoints(&cfg.matter.endpoints)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    validate_matter_identity(&cfg.matter)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    validate_matter_attestation(&cfg.matter)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    validate_tags(&cfg.tags).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    validate_signals(&cfg.signals, &cfg.tags)
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
    use super::*;

    fn ep(kind: &str, source: &str) -> MatterEndpointConfig {
        MatterEndpointConfig {
            kind: kind.to_string(),
            name: String::new(),
            endpoint: None,
            source: source.to_string(),
            sink: String::new(),
            fan_speeds: String::new(),
            scale: 1.0,
            offset: 0.0,
            min: None,
            max: None,
            invert: false,
            occupancy_type: String::new(),
            sources: Default::default(),
            scales: Default::default(),
            hold_ms: None,
            switch_mode: String::new(),
            long_press_ms: None,
            multi_press_ms: None,
            multi_press_max: None,
            debounce_ms: None,
            poll_ms: None,
            allow_mock: false,
        }
    }

    #[test]
    fn matter_endpoints_accept_every_kind_with_a_valid_source() {
        for (kind, k) in MatterEndpointKind::ALL {
            let e = if k.is_actuator() {
                let mut e = ep(kind, "");
                e.sink = "virtual".into();
                e
            } else if k.is_multi_source() {
                let mut e = ep(kind, "");
                e.sources.insert("co2".into(), "builtin:soc_temp_c".into());
                e
            } else {
                ep(kind, "builtin:soc_temp_c")
            };
            assert!(validate_matter_endpoints(&[e]).is_ok(), "{kind}");
        }
    }

    #[test]
    fn matter_endpoints_reject_bad_input() {
        let bad: Vec<(&str, MatterEndpointConfig)> = vec![
            ("unknown kind", ep("toaster", "builtin:x")),
            ("missing source", ep("temperature_sensor", "")),
            ("malformed source", ep("temperature_sensor", "nope")),
            ("poll too fast", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.poll_ms = Some(10);
                e
            }),
            ("zero scale", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.scale = 0.0;
                e
            }),
            ("min >= max", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.min = Some(5.0);
                e.max = Some(5.0);
                e
            }),
            ("NaN min", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.min = Some(f64::NAN);
                e.max = Some(1.0);
                e
            }),
            ("legacy id", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.endpoint = Some(3);
                e
            }),
            ("root id", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.endpoint = Some(0);
                e
            }),
            ("long name", {
                let mut e = ep("temperature_sensor", "builtin:x");
                e.name = "x".repeat(33);
                e
            }),
            ("bad occupancy type", {
                let mut e = ep("occupancy_sensor", "builtin:motion");
                e.occupancy_type = "sonar".into();
                e
            }),
        ];
        for (why, e) in bad {
            assert!(
                validate_matter_endpoints(&[e]).is_err(),
                "should reject: {why}"
            );
        }
    }

    #[test]
    fn matter_endpoints_reject_duplicates_and_accept_new_occupancy_technologies() {
        let mut a = ep("temperature_sensor", "builtin:x");
        a.name = "same".into();
        let mut b = ep("humidity_sensor", "builtin:y");
        b.name = "same".into();
        assert!(
            validate_matter_endpoints(&[a, b]).is_err(),
            "duplicate names"
        );

        let mut a = ep("temperature_sensor", "builtin:x");
        a.endpoint = Some(20);
        let mut b = ep("humidity_sensor", "builtin:y");
        b.endpoint = Some(20);
        assert!(
            validate_matter_endpoints(&[a, b]).is_err(),
            "duplicate pinned ids"
        );

        for tech in [
            "pir",
            "ultrasonic",
            "physical_contact",
            "vision",
            "radar",
            "other",
            "",
        ] {
            let mut e = ep("occupancy_sensor", "builtin:motion");
            e.occupancy_type = tech.to_string();
            assert!(validate_matter_endpoints(&[e]).is_ok(), "{tech}");
        }
    }

    #[test]
    fn actuators_need_a_sink_and_sensors_need_a_source() {
        let mut light = ep("on_off_light", "");
        assert!(
            validate_matter_endpoints(&[light.clone()]).is_err(),
            "actuator with no sink"
        );
        light.sink = "gpio:/dev/gpiochip0:17".into();
        assert!(validate_matter_endpoints(&[light.clone()]).is_ok());
        light.sink = "gpio:/dev/gpiochip0".into();
        assert!(
            validate_matter_endpoints(&[light.clone()]).is_err(),
            "malformed sink"
        );
        let mut mixed = ep("fan", "builtin:motion");
        mixed.sink = "virtual".into();
        assert!(
            validate_matter_endpoints(&[mixed]).is_err(),
            "actuator with a source"
        );
        let mut sensor = ep("temperature_sensor", "builtin:soc_temp_c");
        sensor.sink = "virtual".into();
        assert!(
            validate_matter_endpoints(&[sensor]).is_err(),
            "sensor with a sink"
        );
    }

    #[test]
    fn fan_speeds_must_be_real_and_deliverable_by_the_sink() {
        let fan = |sink: &str, speeds: &str| {
            let mut e = ep("fan", "");
            e.sink = sink.into();
            e.fan_speeds = speeds.into();
            e
        };
        for speeds in ["", "off_high", "off_low_high", "off_low_med_high"] {
            assert!(
                validate_matter_endpoints(&[fan("signal:fan", speeds)]).is_ok(),
                "{speeds}"
            );
            assert!(
                validate_matter_endpoints(&[fan("virtual", speeds)]).is_ok(),
                "{speeds}"
            );
        }
        assert!(validate_matter_endpoints(&[fan("signal:fan", "off_low_med_high_auto")]).is_err());
        // One GPIO line is on or off: only a single-speed fan can use it.
        assert!(validate_matter_endpoints(&[fan("gpio:/dev/gpiochip0:5", "")]).is_ok());
        assert!(validate_matter_endpoints(&[fan("gpio:/dev/gpiochip0:5", "off_high")]).is_ok());
        assert!(
            validate_matter_endpoints(&[fan("gpio:/dev/gpiochip0:5", "off_low_high")]).is_err()
        );
        // Only a fan has speeds.
        let mut light = ep("on_off_light", "");
        light.sink = "virtual".into();
        light.fan_speeds = "off_high".into();
        assert!(validate_matter_endpoints(&[light]).is_err());
        let mut sensor = ep("temperature_sensor", "builtin:soc_temp_c");
        sensor.fan_speeds = "off_high".into();
        assert!(validate_matter_endpoints(&[sensor]).is_err());
    }

    #[test]
    fn generic_switch_options_are_validated() {
        let sw = || ep("generic_switch", "gpio_in:/dev/gpiochip0:17:active_low");
        assert!(validate_matter_endpoints(&[sw()]).is_ok());
        let ok = |f: &dyn Fn(&mut MatterEndpointConfig)| {
            let mut e = sw();
            f(&mut e);
            validate_matter_endpoints(&[e]).is_ok()
        };
        assert!(ok(&|e| e.switch_mode = "momentary".into()));
        assert!(ok(&|e| e.switch_mode = "latching".into()));
        assert!(!ok(&|e| e.switch_mode = "toggle".into()), "unknown mode");
        // Timing knobs: in range accepted, out of range refused.
        assert!(ok(&|e| {
            e.long_press_ms = Some(500);
            e.multi_press_ms = Some(250);
            e.multi_press_max = Some(5);
            e.debounce_ms = Some(0);
        }));
        assert!(!ok(&|e| e.long_press_ms = Some(100)));
        assert!(!ok(&|e| e.long_press_ms = Some(20_000)));
        assert!(!ok(&|e| e.multi_press_ms = Some(50)));
        assert!(!ok(&|e| e.multi_press_max = Some(1)));
        assert!(!ok(&|e| e.multi_press_max = Some(21)));
        assert!(!ok(&|e| e.debounce_ms = Some(600)));
        // The momentary-only knobs would silently do nothing on a latching switch.
        assert!(!ok(&|e| {
            e.switch_mode = "latching".into();
            e.long_press_ms = Some(500);
        }));
        assert!(
            ok(&|e| {
                e.switch_mode = "latching".into();
                e.debounce_ms = Some(50);
            }),
            "debounce applies to both"
        );
        // A switch has to catch a short press, so it polls fast; sensors don't.
        assert_eq!(
            effective_poll_ms(MatterEndpointKind::GenericSwitch, &sw()),
            20
        );
        assert_eq!(
            effective_poll_ms(
                MatterEndpointKind::Temperature,
                &ep("temperature_sensor", "builtin:x")
            ),
            1000
        );
        assert!(ok(&|e| e.poll_ms = Some(5)));
        assert!(!ok(&|e| e.poll_ms = Some(4)));
        assert!(
            !ok(&|e| e.poll_ms = Some(2_000)),
            "too slow to catch a press"
        );
        // ...and none of it is allowed on other kinds.
        for f in [
            (|e: &mut MatterEndpointConfig| e.switch_mode = "momentary".into())
                as fn(&mut MatterEndpointConfig),
            |e| e.long_press_ms = Some(500),
            |e| e.multi_press_ms = Some(300),
            |e| e.multi_press_max = Some(3),
            |e| e.debounce_ms = Some(30),
        ] {
            let mut e = ep("temperature_sensor", "builtin:x");
            f(&mut e);
            assert!(validate_matter_endpoints(&[e]).is_err());
        }
        // A switch needs a source, not a sink.
        let mut with_sink = ep("generic_switch", "");
        with_sink.sink = "virtual".into();
        assert!(validate_matter_endpoints(&[with_sink]).is_err());
        assert!(
            validate_matter_endpoints(&[ep("generic_switch", "")]).is_err(),
            "missing source"
        );
    }

    #[test]
    fn hold_ms_is_for_occupancy_sensors_only_and_bounded() {
        let occ = |hold: Option<u64>| {
            let mut e = ep("occupancy_sensor", "ai:class:person");
            e.hold_ms = hold;
            validate_matter_endpoints(&[e]).is_ok()
        };
        assert!(occ(None));
        assert!(occ(Some(0)));
        assert!(occ(Some(15_000)));
        assert!(occ(Some(3_600_000)));
        assert!(!occ(Some(3_600_001)));
        let mut contact = ep("contact_sensor", "builtin:x");
        contact.hold_ms = Some(1000);
        assert!(
            validate_matter_endpoints(&[contact]).is_err(),
            "a hold makes no sense on a contact"
        );
    }

    #[test]
    fn ai_sources_validate_as_sources() {
        for src in [
            "ai:class:person",
            "ai:class:traffic light",
            "ai:rule:front door",
            "ai:any",
        ] {
            assert!(
                validate_matter_endpoints(&[ep("occupancy_sensor", src)]).is_ok(),
                "{src}"
            );
        }
        for src in [
            "ai:",
            "ai:person",
            "ai:class:",
            "ai:rule:",
            "ai:class",
            "ai:anything",
        ] {
            assert!(
                validate_matter_endpoints(&[ep("occupancy_sensor", src)]).is_err(),
                "{src}"
            );
        }
    }

    #[test]
    fn signals_initial_reads_bools_integers_and_floats() {
        let cfg: SignalsConfig =
            toml::from_str("[initial]\ndoor = true\nlux = 300\ntemp = 22.5\n").unwrap();
        assert_eq!(cfg.initial["door"], SignalInitial::Bool(true));
        assert_eq!(cfg.initial["lux"], SignalInitial::Number(300.0));
        assert_eq!(cfg.initial["temp"], SignalInitial::Number(22.5));
        assert!(validate_signals(&cfg, &[]).is_ok());
        assert!(
            SignalsConfig::default().initial.is_empty(),
            "no section, no initial values"
        );
    }

    #[test]
    fn signals_initial_is_validated() {
        let one = |name: &str, v: SignalInitial| SignalsConfig {
            initial: [(name.to_string(), v)].into(),
        };
        assert!(validate_signals(&one("has space", SignalInitial::Bool(true)), &[]).is_err());
        assert!(validate_signals(&one("", SignalInitial::Bool(true)), &[]).is_err());
        assert!(validate_signals(&one("nan", SignalInitial::Number(f64::NAN)), &[]).is_err());
        let tag: TagConfig =
            toml::from_str("signal = \"boiler\"\nsource = \"modbus/sim/block\"\ntype = \"u16\"\n")
                .unwrap();
        let err = validate_signals(&one("boiler", SignalInitial::Number(1.0)), &[tag]).unwrap_err();
        assert!(err.contains("[[tags]]"), "{err}");
    }

    #[test]
    fn the_default_matter_credentials_are_the_test_ones_and_valid() {
        let cfg = MatterConfig::default();
        assert_eq!((cfg.vendor_id, cfg.product_id), (0xFFF1, 0x8001));
        assert_eq!((cfg.setup_passcode, cfg.discriminator), (20202021, 3840));
        assert_eq!(cfg.attestation, "test");
        assert!(validate_matter_attestation(&cfg).is_ok());
    }

    #[test]
    fn a_config_without_the_new_keys_still_reads_as_the_test_credentials() {
        // A [matter] section written before these keys existed.
        let cfg: MatterConfig = toml::from_str("enabled = true\nattestation = \"test\"\n").unwrap();
        assert_eq!(
            (
                cfg.vendor_id,
                cfg.product_id,
                cfg.setup_passcode,
                cfg.discriminator
            ),
            (0xFFF1, 0x8001, 20202021, 3840)
        );
        assert!(validate_matter_attestation(&cfg).is_ok());
    }

    #[test]
    fn hex_ids_and_a_custom_setup_code_read_from_toml() {
        let cfg: MatterConfig = toml::from_str(
            "enabled = true\nattestation = \"files\"\nvendor_id = 0x1234\nproduct_id = 0x00A1\nsetup_passcode = 31415926\ndiscriminator = 2020\n\
             dac_file = \"a\"\ndac_key_file = \"b\"\npai_file = \"c\"\ncd_file = \"d\"\n",
        )
        .unwrap();
        assert_eq!(
            (
                cfg.vendor_id,
                cfg.product_id,
                cfg.setup_passcode,
                cfg.discriminator
            ),
            (0x1234, 0xA1, 31415926, 2020)
        );
        assert!(validate_matter_attestation(&cfg).is_ok());
    }

    #[test]
    fn a_custom_setup_code_is_fine_with_the_test_certificates() {
        // Securing the pairing code does not need real certificates.
        let cfg = MatterConfig {
            setup_passcode: 31415926,
            discriminator: 100,
            ..MatterConfig::default()
        };
        assert!(validate_matter_attestation(&cfg).is_ok());
    }

    #[test]
    fn another_vendor_id_needs_real_attestation_files() {
        let cfg = MatterConfig {
            vendor_id: 0x1234,
            ..MatterConfig::default()
        };
        let err = validate_matter_attestation(&cfg).unwrap_err();
        assert!(err.contains("attestation = \"files\""), "{err}");
        let cfg = MatterConfig {
            product_id: 7,
            ..MatterConfig::default()
        };
        assert!(validate_matter_attestation(&cfg).is_err());
    }

    #[test]
    fn files_mode_needs_all_four_files_and_test_mode_refuses_stray_ones() {
        let mut cfg = MatterConfig {
            attestation: "files".into(),
            ..MatterConfig::default()
        };
        for (key, set) in [
            ("dac_file", 0),
            ("dac_key_file", 1),
            ("pai_file", 2),
            ("cd_file", 3),
        ] {
            let err = validate_matter_attestation(&cfg).unwrap_err();
            assert!(err.contains(key), "{err}");
            [
                &mut cfg.dac_file,
                &mut cfg.dac_key_file,
                &mut cfg.pai_file,
                &mut cfg.cd_file,
            ][set]
                .push('x');
        }
        assert!(validate_matter_attestation(&cfg).is_ok());
        let stray = MatterConfig {
            dac_file: "x".into(),
            ..MatterConfig::default()
        };
        assert!(validate_matter_attestation(&stray)
            .unwrap_err()
            .contains("dac_file"));
        let bogus = MatterConfig {
            attestation: "certified".into(),
            ..MatterConfig::default()
        };
        assert!(validate_matter_attestation(&bogus)
            .unwrap_err()
            .contains("\"test\" or \"files\""));
    }

    #[test]
    fn the_setup_passcode_and_discriminator_follow_the_spec() {
        let with = |passcode, discriminator| MatterConfig {
            setup_passcode: passcode,
            discriminator,
            ..MatterConfig::default()
        };
        for bad in [
            0,
            100_000_000,
            99_999_999,
            11_111_111,
            12_345_678,
            87_654_321,
            55_555_555,
        ] {
            assert!(
                validate_matter_attestation(&with(bad, 3840)).is_err(),
                "{bad}"
            );
        }
        for good in [1, 20202021, 99_999_998] {
            assert!(
                validate_matter_attestation(&with(good, 3840)).is_ok(),
                "{good}"
            );
        }
        assert!(validate_matter_attestation(&with(20202021, 4095)).is_ok());
        assert!(
            validate_matter_attestation(&with(20202021, 4096)).is_err(),
            "12 bits"
        );
        assert!(validate_matter_attestation(&MatterConfig {
            vendor_id: 0,
            ..MatterConfig::default()
        })
        .is_err());
    }

    #[test]
    fn matter_identity_strings_are_length_and_character_checked() {
        assert!(
            validate_matter_identity(&MatterConfig::default()).is_ok(),
            "all-default identity is valid"
        );
        let mut cfg = MatterConfig {
            vendor_name: "Acme Controls".into(),
            product_name: "Acme Relay Board".into(),
            device_name: "Acme Relay 01".into(),
            ..MatterConfig::default()
        };
        assert!(validate_matter_identity(&cfg).is_ok());
        cfg.product_name = "x".repeat(33);
        assert!(
            validate_matter_identity(&cfg).is_err(),
            "BasicInformation caps these at 32 bytes"
        );
        cfg.product_name = "bad\nname".into();
        assert!(validate_matter_identity(&cfg).is_err());
    }

    #[test]
    fn every_kind_has_a_distinct_sensible_device_type() {
        let mut seen = std::collections::HashSet::new();
        for (name, kind) in MatterEndpointKind::ALL {
            let (id, rev) = kind.device_type();
            assert!(id != 0 && rev >= 1, "{name}");
            assert!(
                seen.insert(id),
                "{name}: device type 0x{id:04X} is already used by another kind"
            );
        }
    }

    fn aq(sources: &[(&str, &str)]) -> MatterEndpointConfig {
        let mut e = ep("air_quality_sensor", "");
        e.sources = sources
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        e
    }

    #[test]
    fn air_quality_sensor_takes_a_sources_table() {
        let ok = |e: MatterEndpointConfig| validate_matter_endpoints(&[e]).is_ok();
        assert!(ok(aq(&[("co2", "push:co2")])));
        assert!(ok(aq(&[
            ("co2", "push:co2"),
            ("pm25", "builtin:x"),
            ("temperature", "push:t"),
            ("humidity", "push:h")
        ])));
        assert!(
            ok(aq(&[("air_quality", "push:level")])),
            "a device-computed level alone is enough"
        );
        // Needs something to grade.
        assert!(!ok(aq(&[])), "empty sources");
        assert!(
            !ok(aq(&[("temperature", "push:t"), ("humidity", "push:h")])),
            "would read Unknown forever"
        );
        assert!(!ok(aq(&[("pm2.5", "push:x")])), "unknown key");
        assert!(!ok(aq(&[("co2", "nope")])), "malformed spec");
        // It is not a single-source kind, and not an actuator.
        let mut with_source = aq(&[("co2", "push:co2")]);
        with_source.source = "push:x".into();
        assert!(!ok(with_source));
        let mut with_sink = aq(&[("co2", "push:co2")]);
        with_sink.sink = "virtual".into();
        assert!(!ok(with_sink));
        // Scales: only for a configured source, and sane.
        let mut scaled = aq(&[("pm25", "sysfs:/x")]);
        scaled.scales.insert("pm25".into(), 0.001);
        assert!(ok(scaled.clone()));
        scaled.scales.insert("co2".into(), 1.0);
        assert!(
            !ok(scaled.clone()),
            "scale for a source that isn't configured"
        );
        scaled.scales.remove("co2");
        scaled.scales.insert("pm25".into(), 0.0);
        assert!(!ok(scaled.clone()), "zero scale");
        scaled.scales.insert("pm25".into(), f64::NAN);
        assert!(!ok(scaled), "NaN scale");
    }

    #[test]
    fn sources_and_scales_are_only_for_multi_source_kinds() {
        let mut t = ep("temperature_sensor", "builtin:x");
        t.sources.insert("co2".into(), "push:co2".into());
        assert!(validate_matter_endpoints(&[t]).is_err());
        let mut t = ep("temperature_sensor", "builtin:x");
        t.scales.insert("co2".into(), 2.0);
        assert!(validate_matter_endpoints(&[t]).is_err());
    }

    #[test]
    fn air_quality_sensor_parses_inline_tables_from_toml() {
        let cfg: MatterConfig = toml::from_str(
            r#"
            enabled = true
            [[endpoints]]
            kind = "air_quality_sensor"
            name = "Living room air"
            sources = { co2 = "push:co2", pm25 = "sysfs:/sys/bus/iio/devices/iio:device0/in_massconcentration_pm2p5_input" }
            scales = { pm25 = 0.001 }
            "#,
        )
        .unwrap();
        let e = &cfg.endpoints[0];
        assert_eq!(e.sources.len(), 2);
        assert_eq!(e.scales["pm25"], 0.001);
        assert!(validate_matter_endpoints(&cfg.endpoints).is_ok());
        assert_eq!(effective_poll_ms(MatterEndpointKind::AirQuality, e), 1000);
    }

    fn tag(signal: &str, data_type: &str) -> TagConfig {
        TagConfig {
            signal: signal.into(),
            source: "modbus/plc1/meter".into(),
            data_type: data_type.into(),
            offset: 0,
            word_order: String::new(),
            field: String::new(),
            scale: 1.0,
            bias: 0.0,
            max_age_s: 60,
        }
    }

    #[test]
    fn tags_validate_types_options_and_uniqueness() {
        let ok = |t: TagConfig| validate_tags(&[t]).is_ok();
        for ty in ["u16", "i16", "u32", "i32", "f32", "bool", "text"] {
            assert!(ok(tag("s", ty)), "{ty}");
        }
        let mut json = tag("s", "json");
        json.field = "data.temperature".into();
        assert!(ok(json.clone()));
        json.field.clear();
        assert!(!ok(json), "json needs a field");
        assert!(!ok(tag("s", "float64")), "unknown type");
        assert!(!ok(tag("bad name", "u16")), "signal name");
        assert!(!ok(tag("", "u16")), "empty signal");
        let mut field_on_binary = tag("s", "u16");
        field_on_binary.field = "x".into();
        assert!(!ok(field_on_binary), "field only for json");
        // word order: only 32-bit types, only big|swap
        let mut swapped = tag("s", "f32");
        swapped.word_order = "swap".into();
        assert!(ok(swapped.clone()));
        swapped.word_order = "middle".into();
        assert!(!ok(swapped));
        let mut on_16 = tag("s", "u16");
        on_16.word_order = "swap".into();
        assert!(
            !ok(on_16),
            "word_order on a 16-bit type would silently do nothing"
        );
        // offsets, scale, expiry
        let mut off = tag("s", "u16");
        off.offset = 4096;
        assert!(ok(off.clone()));
        off.offset = 4097;
        assert!(!ok(off));
        let mut text_off = tag("s", "text");
        text_off.offset = 2;
        assert!(!ok(text_off), "offset only for binary payloads");
        let mut scale = tag("s", "u16");
        scale.scale = 0.0;
        assert!(!ok(scale.clone()));
        scale.scale = f64::NAN;
        assert!(!ok(scale));
        let mut age = tag("s", "u16");
        age.max_age_s = 0;
        assert!(
            !ok(age.clone()),
            "a signal that never expires must be asked for explicitly with a large value"
        );
        age.max_age_s = 86_401;
        assert!(!ok(age));
        // two tags feeding one signal would fight
        assert!(validate_tags(&[tag("same", "u16"), tag("same", "i16")]).is_err());
        assert!(validate_tags(&[tag("a", "u16"), tag("b", "u16")]).is_ok());
    }

    #[test]
    fn tags_parse_from_toml() {
        let cfg: AppConfig = toml::from_str(&format!(
            "{}\n{}",
            include_str!("../config/iiotedge_default.toml"),
            r#"
            [[tags]]
            signal = "meter_power_w"
            source = "modbus/plc1/meter"
            type = "f32"
            offset = 4
            word_order = "swap"
            scale = 1.0
            max_age_s = 30
            [[tags]]
            signal = "garage_temp"
            source = "zigbee2mqtt/garage"
            type = "json"
            field = "temperature"
            "#
        ))
        .unwrap();
        assert_eq!(cfg.tags.len(), 2);
        assert_eq!(cfg.tags[0].data_type, "f32");
        assert_eq!(cfg.tags[0].max_age_s, 30);
        assert_eq!(cfg.tags[1].max_age_s, 60, "default expiry");
        assert!(validate_tags(&cfg.tags).is_ok());
    }

    #[test]
    fn matter_endpoint_default_names_are_kind_and_position() {
        let e = ep("pressure_sensor", "builtin:x");
        assert_eq!(effective_endpoint_name(&e, 2), "pressure_sensor_3");
        let mut named = e.clone();
        named.name = "Boiler".into();
        assert_eq!(effective_endpoint_name(&named, 2), "Boiler");
    }

    #[test]
    fn matter_endpoints_parse_from_toml() {
        let cfg: MatterConfig = toml::from_str(
            r#"
            enabled = true
            [[endpoints]]
            kind = "temperature_sensor"
            name = "Boiler"
            source = "sysfs:/sys/class/hwmon/hwmon0/temp1_input"
            scale = 0.001
            [[endpoints]]
            kind = "occupancy_sensor"
            source = "builtin:motion"
            occupancy_type = "vision"
            [[endpoints]]
            kind = "fan"
            name = "Ceiling fan"
            sink = "signal:fan"
            fan_speeds = "off_low_med_high"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.endpoints.len(), 3);
        assert_eq!(cfg.endpoints[2].fan_speeds, "off_low_med_high");
        assert_eq!(cfg.endpoints[0].scale, 0.001);
        assert_eq!(
            effective_poll_ms(MatterEndpointKind::Temperature, &cfg.endpoints[0]),
            1000,
            "default poll"
        );
        assert!(validate_matter_endpoints(&cfg.endpoints).is_ok());
        // legacy sections still default sensibly when no endpoints are given
        let legacy: MatterConfig = toml::from_str("enabled = true").unwrap();
        assert!(legacy.endpoints.is_empty());
    }

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
