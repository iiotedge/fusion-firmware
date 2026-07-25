// src/config.rs
//
// Every new field carries a serde default so config files written for older
// firmware versions keep parsing — mass-deployed devices must never brick on
// a config schema bump.
use serde::Deserialize;
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
}

fn default_warn_temp_c() -> f64 {
    80.0
}

fn default_metrics_enabled() -> bool {
    true
}
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
}

fn default_codec() -> String {
    "h264".to_string()
}

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
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub windows: Vec<ScheduleWindow>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
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

fn d_widget_w() -> u32 {
    280
}
fn d_widget_h() -> u32 {
    100
}
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
    Ok(())
}
