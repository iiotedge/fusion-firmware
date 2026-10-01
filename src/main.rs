// Matter's handler chain (src/matter/mod.rs) nests one `ChainedHandler<M, H,
// T>` layer per cluster across every device-type module it always
// constructs (camera: 4 clusters, light: 4, onoff: 2, thermostat: 2, plus
// the root endpoint's own long chain) — adding thermostat.rs (Phase 19f)
// pushed the combined generic nesting deep enough that computing the async
// state-machine layout for `InteractionModel::run()` overflows rustc's
// default query recursion limit (128), failing with "queries overflow the
// depth limit" — a real `cargo build` failure `cargo check`/`clippy`/`cargo
// test` do NOT catch (they don't perform this layout computation), found
// only by building the actual binary. This is the standard, accepted fix
// for genuinely deep (not buggy) generic nesting, not a workaround for a
// bug — expect to raise this further as more Matter device types are
// added to the same always-chained pattern.
#![recursion_limit = "256"]

mod ai;
mod audio;
mod cluster;
mod commands;
mod config;
mod core;
mod correlation;
mod footprint;
mod hal;
mod health;
mod homeassistant;
mod identity;
mod matter;
mod media;
mod motion;
mod mqtt_bridge;
mod onboarding;
mod onvif;
mod ptz;
mod radar;
mod runtime_config;
mod schedule;
mod security;
mod snmp;
mod storage;
mod stream;
mod tamper;
mod telemetry;

use crate::ai::engine::InferenceEngine;
use crate::config::{load_config, SystemConfig};
use crate::config::{CameraConfig, StorageConfig};
use crate::core::metrics::Metrics;
use crate::core::ring_buffer::FrameRouter;
use crate::correlation::CorrelationProcessor;
use crate::hal::create_camera;
use crate::storage::clips::ClipExtractor;
use crate::storage::{ChunkRecorder, ChunkTracker, EventIndexer};
use crate::stream::overlay::{self, DetectionOverlay};
use crate::stream::rtsp_server::RtspStreamer;
use crate::stream::widgets;
use crate::tamper::TamperDetector;
use crate::telemetry::Telemetry;

use gstreamer as gst;
use serde_json::json;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn, Level};

const CONFIG_PATH: &str = "config/iiotedge_default.toml";

/// Logging is configured from [system]; config therefore loads first and any
/// config error goes to stderr directly.
fn init_logging(system: &SystemConfig) {
    let level = match system.log_level.to_uppercase().as_str() {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "INFO" => Level::INFO,
        "WARN" => Level::WARN,
        "ERROR" => Level::ERROR,
        other => {
            eprintln!("Unknown system.log_level '{other}', defaulting to INFO");
            Level::INFO
        }
    };

    let builder = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false);
    if system.log_format.to_uppercase() == "JSON" {
        builder.json().init();
    } else {
        builder.init();
    }
}

fn main() {
    // 1. Load Configuration from TOML.
    // A missing or invalid config is a fatal error in a production edge device.
    let mut app_config = match load_config(CONFIG_PATH) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("CRITICAL: Failed to load configuration from {CONFIG_PATH}: {e}");
            process::exit(1);
        }
    };

    // 2a. Structured logging subscriber (JSON for ELK/Datadog, TEXT for
    // local) — installed immediately after config loads, BEFORE identity/
    // runtime-config resolution below: both can log (identity derivation,
    // an applied ai.rules override), and any info!/warn! emitted before a
    // tracing subscriber exists is silently dropped, not just delayed.
    // Confirmed the hard way: a POST /config/ai-rules override's "applying
    // persisted override" log line was missing from a real boot until this
    // moved ahead of runtime_config::resolve() below. The "Booting..."
    // banner itself stays after identity resolution (1b) so it still
    // reports the final, resolved device_id rather than a possibly-empty
    // pre-resolution one.
    init_logging(&app_config.system);

    // 1b. Fleet identity (Phase 12, F10): fills system.device_id from
    // hardware when left empty in config. Resolved before anything below
    // reads device_id, so every existing call site (ONVIF, telemetry,
    // onboarding, commands, ...) gets it without changes.
    app_config.system.device_id = identity::resolve(
        &app_config.system.device_id,
        &app_config.system.identity_file,
    );

    // 1b'. Remote AI-rules override (Phase 20): if a prior MQTT
    // `config_set_ai_rules` / `POST /config/ai-rules` call persisted a rule
    // set, it replaces this file's own `[[ai.rules]]` for this run — same
    // "persist to disk, re-apply at next boot" pattern as device identity
    // above, just for a richer payload. See src/runtime_config.rs.
    runtime_config::resolve(&mut app_config);

    // 2b. Boot banner — after identity/runtime-config resolution (1b/1b')
    // so device_id is the final resolved value, not empty.
    info!(
        device_id = %app_config.system.device_id,
        version = env!("CARGO_PKG_VERSION"),
        git_hash = env!("GIT_HASH"),
        "Booting Fusion Firmware..."
    );
    info!("Target Hardware: {}", app_config.camera.r#type);

    // 1c. Raw config bytes for the footprint's config_hash (Phase 12) — a
    // second small read is simpler and safer than threading raw bytes out
    // of load_config's own "-> AppConfig" signature.
    let config_file_bytes = std::fs::read(CONFIG_PATH).unwrap_or_default();

    // 3. Initialize Zero-Copy Router (queue capacity from config).
    // Acts as a shock-absorber: drops frames if AI/encoder fall behind,
    // preventing OOM crashes.
    let (router, receivers) = FrameRouter::new(app_config.system.queue_capacity);
    // Third consumer for the Matter WebRTC live H.264 tap (src/matter/
    // encoder.rs) — only wired up when the Camera endpoint specifically is
    // enabled, not just the top-level [matter].enabled: a deployment with
    // [matter.onoff] on but [matter.camera] off (this firmware acting as a
    // Matter light switch, not a camera — see src/matter/mod.rs's header)
    // has nothing to drain this queue (encoder::spawn only runs when
    // camera's enabled), so without this check every captured frame would
    // sit in a permanently-full queue, logging "queue full" warnings
    // forever for no reason — confirmed live, not just reasoned about.
    let (router, mut receivers) = if app_config.matter.enabled && app_config.matter.camera.enabled {
        router.with_matter_tap(receivers, app_config.system.queue_capacity)
    } else {
        (router, receivers)
    };

    // 4. Initialize Hardware via the HAL factory (fully config-driven).
    let mut camera = create_camera(&app_config.camera);
    if let Err(e) = camera.initialize() {
        error!("Hardware initialization failed: {}", e);
        process::exit(1);
    }
    camera
        .start_stream()
        .expect("Failed to start camera capture stream");

    // 4b. Access control shared by RTSP and ONVIF; posture logged at boot so
    // an open camera is never a silent surprise.
    let access = security::AccessControl::new(&app_config.security);
    security::log_posture(&access, !app_config.security.command_token.is_empty());
    if app_config.security.api_token.is_empty() {
        warn!("security.api_token is empty — /cluster/status is unauthenticated");
    }
    if app_config.onboarding.enabled && app_config.security.command_token.is_empty() {
        warn!(
            "onboarding is enabled with an empty command_token — /onboarding/info and \
             /onboarding/qr.png (which embed api_token + RTSP/ONVIF credentials) are \
             reachable by anyone on the LAN"
        );
    }
    if app_config.matter.enabled && app_config.security.command_token.is_empty() {
        warn!(
            "matter is enabled with an empty command_token — /onboarding/matter-qr.png \
             (whoever holds it can commission this device into their own Matter fabric) \
             is reachable by anyone on the LAN"
        );
    }

    // 4c. PTZ (pan/tilt/zoom) motor control, off by default. Constructed
    // once and shared by ONVIF (below) and the MQTT command channel
    // (CommandContext, further down) — a PtzController owns an exclusive
    // serial handle, so there must be exactly one instance. Failure (e.g.
    // configured serial device missing) is logged, not fatal: a camera
    // that can't move its mount must still capture, record and stream.
    let ptz = match ptz::PtzController::new(&app_config.ptz) {
        Ok(controller) => controller,
        Err(e) => {
            warn!("PTZ disabled: {e}");
            None
        }
    };

    // 5. ONVIF network services (WS-Discovery + SOAP device/media). Their
    // failure never takes down the video path, so they are not watchdogged.
    let _onvif_handles = onvif::spawn(&app_config, access.clone(), ptz.clone());

    // 5b. Observability: Prometheus /metrics + /healthz (+ /cluster/status
    // once cluster mode is up — the server itself is started further down,
    // after cluster::spawn, so it has a handle to serve that route from).
    let booted = Instant::now();
    let metrics = Metrics::new();

    // 5b'. System health sampler (SoC temp / CPU / memory / throttle) →
    // metrics + health beacon. Best-effort; empty on boards without sysfs.
    let health = health::spawn(metrics.clone(), app_config.system.warn_temp_c);

    // 5b''. Thread-liveness watchdog: workers bump heartbeats; a wedged (but
    // not panicked) thread trips the capture-loop supervisor into exit(2).
    let watchdog = core::watchdog::Watchdog::new(app_config.watchdog.thread_timeout_ms);

    // 5c. Graceful shutdown: SIGTERM/SIGINT set a flag the capture loop
    // honors, so the in-flight recording chunk gets finalized (moov atom)
    // and the telemetry queue drains to disk before exit.
    let shutdown = Arc::new(AtomicBool::new(false));
    #[cfg(unix)]
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        if let Err(e) = signal_hook::flag::register(signal, shutdown.clone()) {
            warn!("Failed to register signal handler {signal}: {e}");
        }
    }

    // 6. Machine-data correlation tap: a Processor in the telemetry engine's
    // ingest path matches southbound events against [correlation] rules;
    // hits pair with frames on the analytics thread.
    let mut processors: Vec<Arc<dyn iiotedge_core::traits::Processor>> = Vec::new();
    let mut correlation_tap: Option<Arc<CorrelationProcessor>> = None;
    let correlation_rx = CorrelationProcessor::new(&app_config.correlation).map(|(tap, rx)| {
        correlation_tap = Some(tap.clone());
        processors.push(tap);
        rx
    });
    // 6a-ii. Smart-home MQTT bridges (Phase 19b): Zigbee2MQTT / Z-Wave JS
    // UI / any JSON-over-MQTT source, fed straight into the SAME
    // correlation tap above via a direct Processor::process call (this
    // isn't a registered iiotedge-lib southbound driver, so there's no
    // engine ingest path to ride) — inert when [correlation] has no
    // matching rules, same as every other southbound source.
    if let Some(tap) = &correlation_tap {
        for source in &app_config.mqtt_bridge {
            mqtt_bridge::spawn(source.clone(), tap.clone());
        }
    } else if !app_config.mqtt_bridge.is_empty() {
        warn!(
            "mqtt_bridge configured but [correlation] is disabled or has no rules \
             — every bridge event would be silently dropped, so none were started. \
             Add at least one [[correlation.rules]] entry to use mqtt_bridge."
        );
    }
    // On-video machine widgets tap the same ingest path for their data.
    let widget_bus = widgets::WidgetFeed::new(&app_config.overlay.widgets).map(|(feed, bus)| {
        processors.push(feed);
        bus
    });

    // 6b. Telemetry: the iiotedge-lib persist-first engine on its own tokio
    // runtime thread — SQLite store-and-forward, MQTT (GDE JSON/Sparkplug B),
    // and the southbound machine drivers declared in config/edge.toml
    // (serial, CAN, Modbus, …). None = disabled or failed; video continues.
    let telemetry = Telemetry::start(&app_config, processors);

    // 6b-ii. Device footprint (Phase 12, F10): published once at boot as a
    // GDE "device_birth" event (mirrors Sparkplug's own NBIRTH) and served
    // at GET /footprint (core/metrics.rs) for anything that'd rather poll
    // than watch the telemetry stream.
    let footprint = Arc::new(footprint::Footprint::build(&app_config, &config_file_bytes));
    if let Some(t) = &telemetry {
        t.publish_json(
            "device_birth",
            serde_json::to_value(footprint.as_ref()).unwrap_or_default(),
        );
    }

    // 6b-iii. SNMP agent (Phase 12, F10), off by default — MIB-II system
    // group + a private enterprise MIB backed by the same `metrics`
    // counters /metrics already serves. See src/snmp/mod.rs. `snmp_ctx`
    // clone kept around: the analytics thread also needs it to send the
    // tamper-alarm trap (see that thread's tamper transition handling).
    let snmp_ctx = snmp::SnmpContext {
        metrics: metrics.clone(),
        footprint: footprint.clone(),
        booted,
        sys_contact: app_config.snmp.sys_contact.clone(),
        sys_location: app_config.snmp.sys_location.clone(),
        telemetry_enabled: app_config.telemetry.enabled,
    };
    snmp::spawn(app_config.snmp.clone(), snmp_ctx.clone());

    // 6c. Evidence actions shared by analytics and the command channel.
    let clip_extractor = ClipExtractor::spawn(&app_config.storage, &app_config.system.device_id);
    let manual_snapshot = Arc::new(AtomicBool::new(false));
    // Peer-triggered re-analysis (F9 cross-device AI): set by the
    // `reanalyze` command (MQTT or, more commonly, a cluster
    // `remote_cmd = "reanalyze"` reaction), consumed by the AI engine
    // thread to bypass its normal fps-limit skip for one frame.
    let reanalyze_requested = Arc::new(AtomicBool::new(false));
    // Remote AI-rules updates (Phase 20, src/runtime_config.rs): set by
    // `config_set_ai_rules` (MQTT or HTTP) after it validates and persists,
    // consumed by the analytics thread once per frame to rebuild
    // `RuleEngine` live — same cross-thread handoff shape as
    // `reanalyze_requested` above, just carrying a payload instead of a flag.
    let rule_update_slot = runtime_config::RuleUpdateSlot::new();

    // 6c'. Evidence export (SD/USB mirror + FTPS upload) and storage health.
    let export_trigger = storage::export::spawn(
        &app_config.storage,
        &app_config.system.device_id,
        metrics.clone(),
    );
    storage::spawn_health_monitor(app_config.storage.clone(), metrics.clone());

    // 6d'. Cloud-push relay (stream_start/stream_stop): pulls this same
    // local RTSP feed and republishes it to media-ingestion-service's
    // MediaMTX on command, over RTSP or WebRTC/WHIP per stream_start's
    // `mode` — see src/stream/relay.rs. Off by default
    // ([cloud_relay].enabled); the local RTSP server above is untouched
    // either way.
    let cloud_relay = Arc::new(stream::relay::StreamRelay::new(
        &app_config.stream,
        &app_config.cloud_relay,
    ));
    let local_rtsp_url = match (
        app_config.security.rtsp_auth,
        app_config.security.users.first(),
    ) {
        (true, Some(user)) => format!(
            "rtsp://{}:{}@127.0.0.1:{}{}",
            user.username, user.password, app_config.stream.rtsp_port, app_config.stream.rtsp_path
        ),
        _ => format!(
            "rtsp://127.0.0.1:{}{}",
            app_config.stream.rtsp_port, app_config.stream.rtsp_path
        ),
    };

    // 6d''. Cluster mode: peer discovery + event sharing with no cloud/broker
    // dependency (WiFi multicast and/or Bluetooth LE — see src/cluster/).
    // None when disabled or no transport could start; the camera runs
    // standalone exactly as before. Created here (earlier than the rest of
    // the cluster wiring below) specifically so `CommandContext` can hold a
    // handle to both — the `test_detect` command needs to publish onto the
    // cluster bus and record into fusion the same way a genuine detection
    // does.
    let cluster = cluster::spawn(
        &app_config.cluster,
        &app_config.system.device_id,
        &app_config.system.facility_id,
    );
    // Cross-device AI detection fusion (F9 Phase C) — its own opt-in
    // ([cluster.fusion].enabled), independent of whether a cluster
    // transport actually started: if cluster mode is off, `correlate_peer`
    // just never gets called (no peer events exist to correlate against),
    // but the AI engine thread below always owns a handle to feed its own
    // detections into via `record_local`, no `cluster.is_some()` branch
    // needed there.
    let fusion = cluster::fusion::DetectionFusion::new(&app_config.cluster.fusion);

    // 6d. MQTT command channel (status / snapshot / clip / config_get /
    // reboot with acks) — same broker + identity as the telemetry engine.
    // A standalone binding (not built inline in the `spawn` call) so it can
    // also be cloned into the cluster_commands executor thread below —
    // peer-issued commands run through the exact same `handle_command` path.
    let command_ctx = commands::CommandContext {
        device_id: app_config.system.device_id.clone(),
        firmware_config_path: CONFIG_PATH.to_string(),
        metrics: metrics.clone(),
        booted,
        manual_snapshot: manual_snapshot.clone(),
        clip_extractor: clip_extractor.clone(),
        export_trigger: export_trigger.clone(),
        command_token: app_config.security.command_token.clone(),
        cloud_relay_enabled: app_config.cloud_relay.enabled,
        relay: cloud_relay,
        local_rtsp_url,
        reanalyze_requested: reanalyze_requested.clone(),
        ai_test_hooks_enabled: app_config.ai.test_hooks_enabled,
        fusion: fusion.clone(),
        cluster: cluster.clone(),
        ptz: ptz.clone(),
        ai_labels: app_config.ai.labels.clone(),
        ai_rules_override_path: app_config.system.ai_rules_override_file.clone(),
        rule_update_slot: rule_update_slot.clone(),
    };
    commands::spawn(&app_config, command_ctx.clone());

    // 6e. Home Assistant MQTT Discovery (Phase 19a) — off by default. Rides
    // the SAME broker/topic as the command channel above so its snapshot/
    // clip button entities work with zero new command-ingestion code (see
    // src/homeassistant.rs's header comment for why that coupling is
    // deliberate, not incidental).
    // Zone names must match motion.rs's own fallback exactly (empty
    // config ⇒ one implicit "frame" zone, src/motion.rs) — HA's
    // discovery topic and the analytics thread's runtime publish topic
    // have to agree on the same object_id or state updates land nowhere.
    let ha_motion_zones: Vec<String> = if !app_config.motion.enabled {
        Vec::new()
    } else if app_config.motion.zones.is_empty() {
        vec!["frame".to_string()]
    } else {
        app_config
            .motion
            .zones
            .iter()
            .map(|z| z.name.clone())
            .collect()
    };
    let ha_bridge = homeassistant::HomeAssistantBridge::spawn(
        &app_config.home_assistant,
        &app_config.telemetry.edge_config,
        &app_config.system.device_id,
        &app_config.security.command_token,
        app_config.ai.rules.iter().map(|r| r.name.clone()).collect(),
        ha_motion_zones,
    );

    // 6f. Radar sensing (Phase 17) — off by default, additive alongside
    // the camera; standalone today (17b analytics only). Cross-modal
    // fusion with camera/AI (17c/17d) is design-only until a real radar
    // backend exists to calibrate extrinsics against — see TODO.md Phase
    // 17c/17d. Cross-device fusion (17e) IS wired here: a fired zone
    // event broadcasts as a cluster "radar_zone" Event and feeds
    // DetectionFusion::record_local_radar the same way the AI engine
    // thread above already feeds record_local.
    if app_config.radar.enabled {
        match radar::create_radar(&app_config.radar) {
            Some(mut radar_source) => {
                let radar_telemetry = telemetry.clone();
                let radar_cluster = cluster.clone();
                let radar_fusion = fusion.clone();
                let radar_device_id = app_config.system.device_id.clone();
                let radar_zones = app_config.radar.zones.clone();
                let radar_heartbeat = watchdog.register("radar");
                let radar_shutdown = shutdown.clone();
                let spawned = thread::Builder::new()
                    .name("radar".to_string())
                    .spawn(move || {
                        // Graceful degradation, same philosophy as a
                        // missing/incompatible AI model: radar is an
                        // optional additive sensor, never a reason to
                        // crash-loop the whole device.
                        if let Err(e) = radar_source.initialize() {
                            error!("Radar unavailable ({e}); continuing without radar sensing");
                            return;
                        }
                        if let Err(e) = radar_source.start() {
                            error!("Radar failed to start ({e}); continuing without radar sensing");
                            return;
                        }
                        let mut analyzer = radar::analytics::RadarAnalyzer::new(&radar_zones);
                        info!(zones = radar_zones.len(), "Radar sensing active");
                        loop {
                            if radar_shutdown.load(Ordering::Relaxed) {
                                info!("Shutdown signal received; stopping radar");
                                let _ = radar_source.stop();
                                break;
                            }
                            radar_heartbeat.beat();
                            let frame = match radar_source.next_frame() {
                                Ok(f) => f,
                                Err(e) => {
                                    warn!("Radar frame read failed: {e}");
                                    continue;
                                }
                            };
                            let health = radar_source.health();
                            if health.is_degraded() {
                                warn!(
                                    interference = health.interference,
                                    blocked = health.blocked,
                                    saturated = health.saturated,
                                    "Radar health degraded"
                                );
                            }
                            if analyzer.is_empty() {
                                continue;
                            }
                            for event in analyzer.evaluate(&frame) {
                                info!(
                                    zone = %event.zone_name,
                                    mode = %event.mode,
                                    velocity_mps = event.velocity_mps,
                                    "Radar zone event"
                                );
                                let payload = json!({
                                    "zone": event.zone_name,
                                    "mode": event.mode,
                                    "track_id": event.track_id,
                                    "class_hint": event.class_hint,
                                    "range_m": event.range_m,
                                    "azimuth_deg": event.azimuth_deg,
                                    "velocity_mps": event.velocity_mps,
                                });
                                if let Some(t) = &radar_telemetry {
                                    t.publish_json("radar_event", payload);
                                }
                                if let Some(f) = &radar_fusion {
                                    f.record_local_radar(&event.zone_name);
                                }
                                if let Some(bus) = &radar_cluster {
                                    bus.publish_event(
                                        "radar_zone",
                                        event.zone_name.clone(),
                                        &radar_device_id,
                                    );
                                }
                            }
                        }
                    });
                if let Err(e) = spawned {
                    warn!("Failed to spawn radar thread: {e}");
                }
            }
            None => {
                warn!(
                    radar_type = %app_config.radar.r#type,
                    "radar.enabled is true but radar.type is not a registered backend \
                     — continuing without radar sensing"
                );
            }
        }
    }

    // 6g. Matter protocol support (Phase 19c/19d) — off by default. Runs
    // on its own OS thread (the Matter node's own async run loop is
    // single-threaded, block_on-driven — see src/matter/mod.rs). Spawned
    // whenever [matter].enabled, regardless of which endpoints
    // (camera/onoff) are actually turned on within it — e.g. an
    // onoff-only ("light switch") deployment still needs the Matter node
    // itself running even though it has no use for the live H.264 tap.
    // `receivers.matter_rx` (the third FrameRouter consumer, only wired
    // up above when [matter.camera] specifically is enabled) is threaded
    // through as an `Option` for exactly that reason — conflating "spawn
    // Matter at all" with "is the camera frame tap present" silently
    // disabled the whole subsystem for onoff-only configs (a real bug,
    // caught live rather than just reasoned about).
    if app_config.matter.enabled {
        matter::spawn(
            app_config.matter.clone(),
            app_config.camera.clone(),
            app_config.stream.clone(),
            app_config.ai.rules.clone(),
            app_config.system.device_id.clone(),
            receivers.matter_rx.take(),
            shutdown.clone(),
        );
    }

    // 7. Periodic device health event (uptime + frame throughput).
    let frames_processed = Arc::new(AtomicU64::new(0));
    if let Some(health_pub) = telemetry.clone() {
        let interval = Duration::from_secs(app_config.telemetry.health_ping_interval_sec.max(5));
        let frames = frames_processed.clone();
        let health_monitor = health.clone();
        let spawned = thread::Builder::new()
            .name("health_beacon".to_string())
            .spawn(move || loop {
                thread::sleep(interval);
                let sys = health_monitor.snapshot();
                health_pub.publish_json(
                    "health",
                    json!({
                        "uptime_s": booted.elapsed().as_secs(),
                        "frames_processed": frames.load(Ordering::Relaxed),
                        "firmware_version": env!("CARGO_PKG_VERSION"),
                        "soc_temp_c": sys.soc_temp_c,
                        "cpu_load_percent": sys.cpu_load_percent,
                        "mem_used_percent": sys.mem_used_percent,
                        "throttled": sys.throttled,
                    }),
                );
            });
        if let Err(e) = spawned {
            warn!("Failed to spawn health beacon: {e}");
        }
    }

    if app_config.system.metrics_enabled {
        // Phase 12: QR device onboarding — see src/onboarding.rs for the
        // payload shape and docs/QR_ONBOARDING.md for the mobile-app
        // integration contract.
        let onboarding = Arc::new(onboarding::OnboardingContext::from_config(&app_config));
        // Remote AI/automation config over HTTP (Phase 20) — the same
        // capability as the MQTT config_get_ai_rules/config_set_ai_rules
        // commands above, reusing the same rule_update_slot so either
        // channel applies through the one running analytics thread.
        let runtime_config_ctx = core::metrics::RuntimeConfigContext {
            ai_labels: app_config.ai.labels.clone(),
            ai_rules_override_path: app_config.system.ai_rules_override_file.clone(),
            static_config_path: CONFIG_PATH.to_string(),
            rule_update_slot: rule_update_slot.clone(),
            command_token: app_config.security.command_token.clone(),
        };
        // Matter pairing QR (Phase 19c), exposed the same "scan a QR to add
        // this device" way the app-onboarding QR already is — computed
        // once here (pure function of device_id + fixed test commissioning
        // data, no need to reach into the live Matter thread) rather than
        // per-request, since it leaks a small string each call.
        let matter_qr = if app_config.matter.enabled {
            match matter::setup_qr_text(&app_config.system.device_id) {
                Ok(text) => Some(text),
                Err(e) => {
                    warn!("Matter QR onboarding endpoint disabled: {e}");
                    None
                }
            }
        } else {
            None
        };
        core::metrics::spawn_server(
            metrics.clone(),
            app_config.system.metrics_port,
            booted,
            env!("CARGO_PKG_VERSION"),
            cluster.clone(),
            onboarding,
            footprint.clone(),
            runtime_config_ctx,
            matter_qr,
        );
    }

    if let Some(cluster) = &cluster {
        let cluster_metrics = metrics.clone();
        let cluster_view = cluster.view();
        let spawned = thread::Builder::new()
            .name("cluster_metrics".to_string())
            .spawn(move || loop {
                cluster_metrics
                    .cluster_peer_count
                    .set(cluster_view.peer_count() as i64);
                cluster_metrics
                    .cluster_is_leader
                    .set(i64::from(cluster_view.is_leader()));
                thread::sleep(Duration::from_secs(5));
            });
        if let Err(e) = spawned {
            warn!("Failed to spawn cluster metrics updater: {e}");
        }

        // Single consumer of drain_peer_events(): dispatches to configured
        // [[cluster.reactions]] (e.g. a tamper alarm on another camera
        // triggers a confirmatory local snapshot, and/or — remote_cmd set —
        // tells another peer to run a command: agentic, rule-based
        // cross-device control, no LLM involved) AND, if enabled,
        // cross-device AI detection fusion (see cluster/fusion.rs). Both
        // live in one thread because peer_events_rx only delivers each
        // event to ONE consumer — draining it from two independent threads
        // would race them against each other. "*" matches any peer event
        // kind for reactions.
        let reactions = app_config.cluster.reactions.clone();
        if !reactions.is_empty() || fusion.is_some() {
            let cluster_rx = cluster.clone();
            let reaction_snapshot = manual_snapshot.clone();
            let reaction_fusion = fusion.clone();
            let reaction_telemetry = telemetry.clone();
            let reaction_device_id = app_config.system.device_id.clone();
            // Remote commands run through the same bearer-token check as
            // MQTT ones (commands::handle_command) on the receiving device,
            // so this only works fleet-wide when peers share a
            // [security].command_token — the mesh has no separate secret
            // store of its own, by design.
            let reaction_token = app_config.security.command_token.clone();
            let spawned = thread::Builder::new()
                .name("cluster_reactions".to_string())
                .spawn(move || loop {
                    for peer_event in cluster_rx.drain_peer_events() {
                        for reaction in &reactions {
                            let matches =
                                reaction.on_kind == "*" || reaction.on_kind == peer_event.kind;
                            if !matches {
                                continue;
                            }
                            info!(
                                peer = %peer_event.source_device,
                                kind = %peer_event.kind,
                                summary = %peer_event.summary,
                                "Cluster peer event matched a local reaction"
                            );
                            if reaction.snapshot {
                                reaction_snapshot.store(true, Ordering::Relaxed);
                            }
                            if let Some(remote_cmd) = &reaction.remote_cmd {
                                cluster_rx.send_command(
                                    &reaction.remote_target,
                                    json!({
                                        "cmd": remote_cmd,
                                        "id": uuid::Uuid::new_v4().to_string(),
                                        "token": reaction_token,
                                    }),
                                );
                            }
                        }
                        // Cross-device AI detection fusion: does a peer's
                        // "ai_event" match one of OUR OWN recent detections
                        // (same label, within [cluster.fusion].tolerance_ms)?
                        // If so, the same real-world object was very likely
                        // seen by both cameras — corroborating evidence, not
                        // two disconnected events.
                        if peer_event.kind == "ai_event" {
                            if let Some(fusion) = &reaction_fusion {
                                if let Some(local_lag) = fusion.correlate_peer(&peer_event.summary)
                                {
                                    info!(
                                        peer = %peer_event.source_device,
                                        label = %peer_event.summary,
                                        local_lag_ms = local_lag.as_millis() as u64,
                                        "Cross-device detection fusion: same object seen by multiple cameras"
                                    );
                                    if let Some(t) = &reaction_telemetry {
                                        t.publish_json(
                                            "fused_detection",
                                            json!({
                                                "label": peer_event.summary,
                                                "peer_device": peer_event.source_device,
                                                "local_device": reaction_device_id,
                                                "local_lag_ms": local_lag.as_millis() as u64,
                                            }),
                                        );
                                    }
                                }
                            }
                        }
                        // Cross-device radar zone fusion (Phase 17e) —
                        // same corroboration principle as ai_event above,
                        // matched against the radar-only fusion queue
                        // (record_local_radar/correlate_peer_radar) so a
                        // radar zone name can never accidentally
                        // same-string-match an unrelated AI class label.
                        if peer_event.kind == "radar_zone" {
                            if let Some(fusion) = &reaction_fusion {
                                if let Some(local_lag) =
                                    fusion.correlate_peer_radar(&peer_event.summary)
                                {
                                    info!(
                                        peer = %peer_event.source_device,
                                        zone = %peer_event.summary,
                                        local_lag_ms = local_lag.as_millis() as u64,
                                        "Cross-device radar fusion: same object seen by multiple radar nodes"
                                    );
                                    if let Some(t) = &reaction_telemetry {
                                        t.publish_json(
                                            "fused_radar_zone",
                                            json!({
                                                "zone": peer_event.summary,
                                                "peer_device": peer_event.source_device,
                                                "local_device": reaction_device_id,
                                                "local_lag_ms": local_lag.as_millis() as u64,
                                            }),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(300));
                });
            if let Err(e) = spawned {
                warn!("Failed to spawn cluster reaction handler: {e}");
            }
        }

        // Execute commands peers send us (see [cluster].accept_remote_commands,
        // off by default) — already filtered to ones addressed to this node
        // by cluster::run_bus, run through the identical audited
        // commands::handle_command path MQTT commands use (same token
        // check, same ack shape), reboot included.
        if app_config.cluster.accept_remote_commands {
            let cluster_cmds = cluster.clone();
            let cmd_ctx = command_ctx.clone();
            let spawned = thread::Builder::new()
                .name("cluster_commands".to_string())
                .spawn(move || loop {
                    for peer_cmd in cluster_cmds.drain_peer_commands() {
                        let raw = serde_json::to_vec(&peer_cmd.payload).unwrap_or_default();
                        let ack = commands::handle_command(&raw, &cmd_ctx);
                        info!(peer = %peer_cmd.source_device, "Executed cluster peer command");
                        let reboot = ack.get("reboot").and_then(|v| v.as_bool()).unwrap_or(false);
                        cluster_cmds.send_command_ack(ack);
                        if reboot {
                            thread::sleep(Duration::from_millis(500));
                            info!("Reboot command (via cluster peer) honored; exiting for supervisor restart");
                            std::process::exit(3);
                        }
                    }
                    thread::sleep(Duration::from_millis(300));
                });
            if let Err(e) = spawned {
                warn!("Failed to spawn cluster command executor: {e}");
            }
        }
    }

    // Shared AI-detection overlay state: written by the analytics thread,
    // burned into frames by the media thread (with TTL expiry).
    let detection_overlay = Arc::new(DetectionOverlay::new(app_config.overlay.ai_box_ttl_ms));
    // True while any tamper condition is alarmed — media thread draws a
    // full-frame warning border.
    let tamper_active = Arc::new(AtomicBool::new(false));
    // True while motion is active (any zone) — gates motion-mode recording.
    let motion_active = Arc::new(AtomicBool::new(false));
    // Which NVR chunk is currently being written (event↔evidence resolution).
    let chunk_tracker = ChunkTracker::new();

    // Recording schedule (shift/calendar): a background evaluator flips this
    // so the media thread's record decision is a cheap atomic read.
    let schedule_armed = Arc::new(AtomicBool::new(true));
    {
        let sched = schedule::Schedule::new(&app_config.schedule);
        let armed = schedule_armed.clone();
        armed.store(sched.armed_now(), Ordering::Relaxed);
        let spawned = thread::Builder::new()
            .name("schedule".to_string())
            .spawn(move || loop {
                armed.store(sched.armed_now(), Ordering::Relaxed);
                thread::sleep(Duration::from_secs(15));
            });
        if let Err(e) = spawned {
            warn!("Failed to spawn schedule evaluator: {e}");
        }
    }

    // ==========================================
    // THREAD 1: VIDEO ANALYTICS (AI INFERENCE + TAMPER DETECTION)
    // ==========================================
    let ai_rx = receivers.ai_rx;
    let ai_cfg = app_config.ai.clone();
    let correlation_tolerance_ms = app_config.correlation.tolerance_ms.max(1);
    let tamper_cfg = app_config.tamper.clone();
    let motion_cfg = app_config.motion.clone();
    let ai_camera_cfg = app_config.camera.clone();
    let ai_storage_cfg = app_config.storage.clone();
    let ai_device_id = app_config.system.device_id.clone();
    let ai_telemetry = telemetry.clone();
    let ai_detections = detection_overlay.clone();
    let analytics_manual_snapshot = manual_snapshot.clone();
    let analytics_tamper_flag = tamper_active.clone();
    let analytics_motion_flag = motion_active.clone();
    let analytics_chunk_tracker = chunk_tracker.clone();
    let analytics_metrics = metrics.clone();
    let analytics_heartbeat = watchdog.register("analytics");
    let analytics_cluster = cluster.clone();
    let analytics_fusion = fusion.clone();
    let analytics_reanalyze = reanalyze_requested.clone();
    let analytics_snmp_cfg = app_config.snmp.clone();
    let analytics_snmp_ctx = snmp_ctx.clone();
    let analytics_ha = ha_bridge.clone();
    let analytics_rule_update_slot = rule_update_slot.clone();

    let ai_handle = thread::Builder::new()
        .name("ai_engine".to_string())
        .spawn(move || {
            // CPU Pinning: ONLY executes if compiled for the Linux target
            #[cfg(target_os = "linux")]
            {
                if core_affinity::set_for_current(core_affinity::CoreId { id: 2 }) {
                    info!("AI Thread strictly pinned to CPU Core 2");
                }
            }

            // Tamper analytics run even when AI inference is off — a camera
            // that can't see is a security incident regardless of ML.
            let mut tamper_detector = if tamper_cfg.enabled {
                match TamperDetector::new(&tamper_cfg, &ai_camera_cfg) {
                    Ok(det) => {
                        info!("Tamper detection active (blackout/blinding/occlusion/freeze/scene-change)");
                        Some(det)
                    }
                    Err(e) => {
                        warn!("Tamper detection unavailable: {e}");
                        None
                    }
                }
            } else {
                None
            };

            let mut motion_detector = motion::MotionDetector::new(&motion_cfg, &ai_camera_cfg);
            if motion_detector.is_some() {
                info!("Zone motion detection active");
            }

            // Customizable AI detection rules (Phase 16): zone presence,
            // line crossing, loitering. Independent of ai_cfg.enabled at
            // construction — an empty rule set costs nothing to iterate,
            // and evaluate() is only ever called alongside real AiEvents
            // below, which only exist when inference actually ran.
            let mut rule_engine = ai::rules::RuleEngine::new(&ai_cfg, &ai_camera_cfg);
            let mut rule_webhooks = ai_cfg
                .rules
                .iter()
                .any(|r| r.actions.iter().any(|a| a == "webhook"))
                .then(ai::actions::WebhookDispatcher::spawn);
            if !rule_engine.is_empty() {
                info!(
                    rules = ai_cfg.rules.iter().filter(|r| r.enabled).count(),
                    "Customizable AI detection rules active"
                );
            }

            // Graceful degradation: a missing/incompatible model must never
            // crash-loop the camera — streaming and recording continue.
            let mut engine = if ai_cfg.enabled {
                match InferenceEngine::new(&ai_cfg, &ai_camera_cfg) {
                    Ok(engine) => Some(engine),
                    Err(e) => {
                        error!(
                            "AI engine unavailable ({e}); continuing WITHOUT inference — \
                             video streaming and recording are unaffected"
                        );
                        None
                    }
                }
            } else {
                info!("AI inference disabled by config");
                None
            };

            if tamper_detector.is_none()
                && motion_detector.is_none()
                && engine.is_none()
                && correlation_rx.is_none()
            {
                info!("No analytics enabled; draining frames");
                while ai_rx.recv().is_ok() {
                    analytics_heartbeat.beat();
                }
                return;
            }

            // Event evidence: JSONL index (event → chunk/snapshot),
            // rate-limited JPEG snapshots, and pre/post-roll clip extraction.
            let evidence = (ai_storage_cfg.enabled || ai_storage_cfg.snapshots_enabled)
                .then(|| EventIndexer::new(&ai_storage_cfg.path, analytics_chunk_tracker));
            let mut last_snapshot: Option<Instant> = None;
            let mut last_manual_snapshot: Option<Instant> = None;
            let mut last_rule_snapshot: Option<Instant> = None;

            while let Ok(frame) = ai_rx.recv() {
                analytics_heartbeat.beat();

                // Remote AI-rules update (Phase 20): a validated, persisted
                // rule set from `config_set_ai_rules` (MQTT) or
                // `POST /config/ai-rules` is waiting — rebuild RuleEngine
                // from it now rather than waiting for a restart. Checked
                // every frame but only ever `Some` right after a remote
                // change, so this is a no-op `Option::take()` the rest of
                // the time.
                if let Some(new_rules) = analytics_rule_update_slot.take_pending() {
                    let mut updated_ai_cfg = ai_cfg.clone();
                    let new_needs_webhooks = new_rules
                        .iter()
                        .any(|r| r.actions.iter().any(|a| a == "webhook"));
                    updated_ai_cfg.rules = new_rules;
                    rule_engine = ai::rules::RuleEngine::new(&updated_ai_cfg, &ai_camera_cfg);
                    if rule_webhooks.is_none() && new_needs_webhooks {
                        rule_webhooks = Some(ai::actions::WebhookDispatcher::spawn());
                    }
                    info!(
                        rules = updated_ai_cfg.rules.iter().filter(|r| r.enabled).count(),
                        "AI detection rules updated live from a remote config change"
                    );
                }

                // SAFETY: the HAL guarantees data_ptr/size describe a live
                // frame for the duration of this loop iteration.
                let pixels = unsafe { std::slice::from_raw_parts(frame.data_ptr, frame.size) };

                // Manual snapshot (command channel): take it on this frame.
                if analytics_manual_snapshot.swap(false, Ordering::Relaxed) {
                    let snapshot = capture_snapshot(
                        pixels,
                        &ai_camera_cfg,
                        &ai_storage_cfg,
                        &ai_device_id,
                        "manual",
                        &mut last_manual_snapshot,
                    );
                    let payload = json!({
                        "reason": "command",
                        "frame_id": frame.id,
                        "capture_timestamp_ns": frame.timestamp_ns,
                        "snapshot": snapshot,
                    });
                    if let Some(index) = &evidence {
                        index.record("manual_snapshot", snapshot.as_deref(), &payload);
                    }
                    if let Some(t) = &ai_telemetry {
                        t.publish_json("snapshot_event", payload);
                    }
                }

                // Machine-data correlation: pair pending hits with THIS frame
                // — the frame on screen when the machine event arrived. Both
                // timestamps and their delta ride in the correlated event so
                // the platform can judge alignment quality.
                if let Some(corr_rx) = &correlation_rx {
                    while let Ok(hit) = corr_rx.try_recv() {
                        let delta_ms = (frame.timestamp_ns as i128
                            - hit.machine_timestamp_ns as i128)
                            .unsigned_abs()
                            / 1_000_000;
                        let within = delta_ms as u64 <= correlation_tolerance_ms;
                        let reason = format!("corr_{}", hit.rule);
                        let snapshot = if hit.snapshot {
                            capture_snapshot(
                                pixels,
                                &ai_camera_cfg,
                                &ai_storage_cfg,
                                &ai_device_id,
                                &reason,
                                &mut last_snapshot,
                            )
                        } else {
                            None
                        };
                        let payload = json!({
                            "rule": hit.rule,
                            "source_id": hit.source_id,
                            "machine_timestamp_ns": hit.machine_timestamp_ns,
                            "frame_id": frame.id,
                            "capture_timestamp_ns": frame.timestamp_ns,
                            "delta_ms": delta_ms as u64,
                            "within_tolerance": within,
                            "machine_payload": hit.payload_preview,
                            "snapshot": snapshot,
                        });
                        info!(
                            rule = %hit.rule,
                            source = %hit.source_id,
                            delta_ms = delta_ms as u64,
                            within_tolerance = within,
                            "Machine event correlated to frame"
                        );
                        if let Some(index) = &evidence {
                            index.record("correlation", snapshot.as_deref(), &payload);
                        }
                        if hit.clip {
                            if let Some(clips) = &clip_extractor {
                                clips.request(&reason, payload.clone());
                            }
                        }
                        if let Some(t) = &ai_telemetry {
                            t.publish_json("correlated_event", payload);
                        }
                        if let Some(bus) = &analytics_cluster {
                            bus.publish_event("correlated_event", hit.rule.clone(), &ai_device_id);
                        }
                    }
                }

                if let Some(detector) = tamper_detector.as_mut() {
                    for transition in detector.analyze(pixels) {
                        if transition.active {
                            analytics_metrics.tamper_alarms.inc();
                            warn!(
                                kind = transition.kind.as_str(),
                                value = transition.value,
                                threshold = transition.threshold,
                                "TAMPER ALARM"
                            );
                        } else {
                            info!(kind = transition.kind.as_str(), "Tamper recovered");
                        }
                        let snapshot = if transition.active {
                            capture_snapshot(
                                pixels,
                                &ai_camera_cfg,
                                &ai_storage_cfg,
                                &ai_device_id,
                                transition.kind.as_str(),
                                &mut last_snapshot,
                            )
                        } else {
                            None
                        };
                        let payload = json!({
                            "kind": transition.kind.as_str(),
                            "active": transition.active,
                            "value": transition.value,
                            "threshold": transition.threshold,
                            "frame_id": frame.id,
                            "capture_timestamp_ns": frame.timestamp_ns,
                            "snapshot": snapshot,
                        });
                        if let Some(index) = &evidence {
                            index.record("tamper", snapshot.as_deref(), &payload);
                        }
                        if transition.active {
                            if let Some(clips) = &clip_extractor {
                                clips.request(
                                    &format!("tamper_{}", transition.kind.as_str()),
                                    payload.clone(),
                                );
                            }
                        }
                        if let Some(t) = &ai_telemetry {
                            t.publish_json("tamper_event", payload);
                        }
                        if let Some(ha) = &analytics_ha {
                            ha.tamper(transition.active);
                        }
                        if transition.active {
                            if let Some(bus) = &analytics_cluster {
                                bus.publish_event(
                                    "tamper_event",
                                    transition.kind.as_str().to_string(),
                                    &ai_device_id,
                                );
                            }
                            // SNMP trap (Phase 12, F10) — no-op when
                            // [snmp].trap_host is empty (send_trap's own
                            // early return), so this costs nothing on the
                            // far more common "SNMP not deployed" path.
                            snmp::send_trap(
                                &analytics_snmp_cfg,
                                &analytics_snmp_ctx,
                                snmp::TAMPER_ALARM_TRAP,
                                Vec::new(),
                            );
                        }
                    }
                    let active = detector.any_active();
                    analytics_tamper_flag.store(active, Ordering::Relaxed);
                    analytics_metrics.tamper_active.set(i64::from(active));
                }

                // Zone motion detection → motion_event + recording gate.
                if let Some(detector) = motion_detector.as_mut() {
                    for transition in detector.analyze(pixels) {
                        info!(
                            zone = %transition.zone,
                            active = transition.active,
                            score = transition.score,
                            "Motion {}",
                            if transition.active { "started" } else { "ended" }
                        );
                        let snapshot = if transition.active {
                            capture_snapshot(
                                pixels,
                                &ai_camera_cfg,
                                &ai_storage_cfg,
                                &ai_device_id,
                                &format!("motion_{}", transition.zone),
                                &mut last_snapshot,
                            )
                        } else {
                            None
                        };
                        let payload = json!({
                            "zone": transition.zone,
                            "active": transition.active,
                            "score": transition.score,
                            "frame_id": frame.id,
                            "capture_timestamp_ns": frame.timestamp_ns,
                            "snapshot": snapshot,
                        });
                        if let Some(index) = &evidence {
                            index.record("motion", snapshot.as_deref(), &payload);
                        }
                        if let Some(t) = &ai_telemetry {
                            t.publish_json("motion_event", payload);
                        }
                        if let Some(ha) = &analytics_ha {
                            ha.motion(&transition.zone, transition.active);
                        }
                        if transition.active {
                            if let Some(bus) = &analytics_cluster {
                                bus.publish_event("motion_event", transition.zone.clone(), &ai_device_id);
                            }
                        }
                    }
                    analytics_motion_flag.store(detector.any_active(), Ordering::Relaxed);
                }

                let Some(engine) = engine.as_mut() else {
                    continue;
                };
                // Peer-triggered re-analysis (F9 cross-device AI): a
                // neighboring camera flagged something, so this frame
                // bypasses the normal ai.inference_fps_limit skip.
                if analytics_reanalyze.swap(false, Ordering::Relaxed) {
                    debug!(frame = frame.id, "Peer-triggered re-analysis honored");
                    engine.force_next();
                }
                match engine.run_inference(&frame) {
                    Ok(events) => {
                        if events.is_empty() {
                            continue;
                        }
                        info!(
                            "AI detected {} object(s) in frame {}",
                            events.len(),
                            frame.id
                        );
                        analytics_metrics.ai_detections.inc_by(events.len() as u64);
                        // Feed the OSD so boxes appear on the live stream and
                        // in recorded chunks.
                        ai_detections.update(&events);
                        // One snapshot per detection batch, rate-limited; the
                        // clip extractor debounces per-label internally.
                        let snapshot = capture_snapshot(
                            pixels,
                            &ai_camera_cfg,
                            &ai_storage_cfg,
                            &ai_device_id,
                            "ai",
                            &mut last_snapshot,
                        );
                        if let Some(first) = events.first() {
                            if let Some(clips) = &clip_extractor {
                                clips.request(
                                    &format!("ai_{}", first.label),
                                    json!({
                                        "label": first.label,
                                        "confidence": first.confidence,
                                        "detections": events.len(),
                                        "frame_id": frame.id,
                                    }),
                                );
                            }
                            // Cross-device fusion (F9 Phase C): remember our
                            // own detection so a peer reporting the same
                            // label shortly after correlates against it —
                            // see cluster/fusion.rs.
                            if let Some(fusion) = &analytics_fusion {
                                fusion.record_local(&first.label);
                            }
                            // One compact relay per batch (not per detection)
                            // — keeps the cluster bus light, especially over
                            // the BLE transport's small payload budget.
                            if let Some(bus) = &analytics_cluster {
                                bus.publish_event("ai_event", first.label.clone(), &ai_device_id);
                            }
                        }
                        // Persist-first publish: capture timestamp rides along
                        // so machine-data correlation (Phase 7) can match
                        // text events to exact frames.
                        for event in &events {
                            let payload = json!({
                                "label": event.label,
                                "confidence": event.confidence,
                                "x": event.x,
                                "y": event.y,
                                "w": event.w,
                                "h": event.h,
                                "frame_id": frame.id,
                                "capture_timestamp_ns": frame.timestamp_ns,
                                "snapshot": snapshot,
                            });
                            if let Some(index) = &evidence {
                                index.record("ai", snapshot.as_deref(), &payload);
                            }
                            if let Some(t) = &ai_telemetry {
                                t.publish_json("ai_event", payload);
                            }
                        }

                        // Customizable AI detection rules (Phase 16): zone
                        // presence, line crossing, loitering, each with
                        // its own action list.
                        if !rule_engine.is_empty() {
                            for rule_event in rule_engine.evaluate(&events) {
                                info!(
                                    rule = %rule_event.rule_name,
                                    mode = %rule_event.mode,
                                    class = %rule_event.class,
                                    "AI rule matched"
                                );
                                let payload = json!({
                                    "rule": rule_event.rule_name,
                                    "mode": rule_event.mode,
                                    "class": rule_event.class,
                                    "confidence": rule_event.confidence,
                                    "x": rule_event.x,
                                    "y": rule_event.y,
                                    "w": rule_event.w,
                                    "h": rule_event.h,
                                    "frame_id": frame.id,
                                    "capture_timestamp_ns": frame.timestamp_ns,
                                });
                                // telemetry_event is always implicit,
                                // regardless of the rule's own `actions`.
                                if let Some(t) = &ai_telemetry {
                                    t.publish_json("rule_event", payload.clone());
                                }
                                if let Some(ha) = &analytics_ha {
                                    ha.rule(&rule_event.rule_name);
                                }
                                for action in &rule_event.actions {
                                    match action.as_str() {
                                        "snapshot" => {
                                            // Own rate-limit state so a
                                            // rule firing doesn't contend
                                            // with the AI batch's own
                                            // snapshot cadence above.
                                            capture_snapshot(
                                                pixels,
                                                &ai_camera_cfg,
                                                &ai_storage_cfg,
                                                &ai_device_id,
                                                &format!("rule_{}", rule_event.rule_name),
                                                &mut last_rule_snapshot,
                                            );
                                        }
                                        "clip" => {
                                            if let Some(clips) = &clip_extractor {
                                                clips.request(
                                                    &format!("rule_{}", rule_event.rule_name),
                                                    payload.clone(),
                                                );
                                            }
                                        }
                                        "cluster_broadcast" => {
                                            if let Some(bus) = &analytics_cluster {
                                                bus.publish_event(
                                                    "rule_event",
                                                    rule_event.rule_name.clone(),
                                                    &ai_device_id,
                                                );
                                            }
                                        }
                                        "webhook" => {
                                            if let Some(dispatcher) = &rule_webhooks {
                                                dispatcher.request(
                                                    &rule_event.webhook_url,
                                                    payload.clone(),
                                                );
                                            }
                                        }
                                        "gpio_output" => {
                                            ai::actions::pulse_gpio(
                                                rule_event.gpio_chip.clone(),
                                                rule_event.gpio_line,
                                                rule_event.gpio_pulse_ms,
                                            );
                                        }
                                        _ => {} // rejected at config load
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => error!("AI Inference failed on frame {}: {}", frame.id, e),
                }
            }
        })
        .unwrap();

    // ==========================================
    // THREAD 2: MEDIA STREAMER (RTSP) + NVR CHUNK RECORDER + OSD
    // ==========================================
    let stream_rx = receivers.stream_rx;
    let media_config = app_config.clone();
    let media_detections = detection_overlay.clone();
    let media_tamper_flag = tamper_active.clone();
    let media_motion_flag = motion_active.clone();
    let media_schedule_armed = schedule_armed.clone();
    let media_widget_bus = widget_bus.clone();
    let media_access = access.clone();
    let media_heartbeat = watchdog.register("media");
    let media_chunk_tracker = chunk_tracker.clone();

    let stream_handle = thread::Builder::new()
        .name("video_encoder".to_string())
        .spawn(move || {
            // CPU Pinning: ONLY executes if compiled for the Linux target
            #[cfg(target_os = "linux")]
            {
                if core_affinity::set_for_current(core_affinity::CoreId { id: 3 }) {
                    info!("Encoder Thread strictly pinned to CPU Core 3");
                }
            }

            if !media_config.stream.enabled && !media_config.storage.enabled {
                info!("Streaming and recording disabled by config; draining frames");
                while stream_rx.recv().is_ok() {}
                return;
            }

            // Overlay fragment is probed against the GStreamer registry, so
            // init must precede it (idempotent — the sinks init again).
            gst::init().expect("Failed to initialize GStreamer");
            let text_overlays = overlay::text_fragment(&media_config.overlay, &media_config.system);

            // Microphone (optional): one capture, fanned out to RTSP + chunks.
            let audio_plan = audio::start(&media_config.audio);

            // Encoder choice, codec, overlays, audio and pipeline shape all
            // come from config — nothing platform-specific here.
            let streamer = media_config.stream.enabled.then(|| {
                RtspStreamer::new(
                    &media_config.camera,
                    &media_config.stream,
                    &text_overlays,
                    audio_plan.as_ref(),
                    &media_access,
                )
                .expect("Failed to initialize RTSP streaming pipeline")
            });
            let mut recorder = media_config.storage.enabled.then(|| {
                ChunkRecorder::new(
                    &media_config,
                    &text_overlays,
                    media_chunk_tracker,
                    audio_plan.as_ref(),
                )
                .expect("Failed to initialize chunk recorder")
            });

            let draw_ai_boxes = media_config.overlay.enabled && media_config.overlay.show_ai_boxes;
            let draw_tamper_border =
                media_config.overlay.enabled && media_config.tamper.overlay_border;
            let frame_layout = overlay::FrameLayout::from_format(
                &media_config.camera.format,
                media_config.camera.width,
                media_config.camera.height,
            );
            // Thickness scales with resolution (~1.5% of height, min 8 px).
            let tamper_border_px = ((media_config.camera.height / 64).max(8)) as usize;

            // Recording gate: continuous / motion / schedule / both.
            let record_mode = media_config.storage.record_mode.to_lowercase();
            let motion_post_roll =
                Duration::from_secs(media_config.storage.motion_post_roll_s.max(1));
            let mut last_motion_at: Option<Instant> = None;

            while let Ok(frame) = stream_rx.recv() {
                media_heartbeat.beat();
                // Materialize the frame once; burn AI boxes into it so the
                // live stream and the recorded evidence are pixel-identical.
                // SAFETY: the HAL guarantees data_ptr/size describe a live
                // frame for the duration of this call.
                let mut bytes =
                    unsafe { std::slice::from_raw_parts(frame.data_ptr, frame.size) }.to_vec();
                if let Some(layout) = frame_layout {
                    if draw_ai_boxes {
                        let boxes = media_detections.active_boxes();
                        if !boxes.is_empty() {
                            overlay::draw_boxes(&mut bytes, layout, &boxes);
                        }
                    }
                    // Visible tamper warning on live view and evidence alike.
                    if draw_tamper_border && media_tamper_flag.load(Ordering::Relaxed) {
                        overlay::draw_border(&mut bytes, layout, tamper_border_px);
                    }
                    // Live machine-data widgets (sparkline/gauge/value).
                    if media_config.overlay.enabled {
                        if let Some(bus) = &media_widget_bus {
                            widgets::draw_widgets(
                                &mut bytes,
                                layout,
                                &media_config.overlay.widgets,
                                bus,
                            );
                        }
                    }
                }
                // Refcounted buffer: both sinks share the same allocation.
                let buffer = gst::Buffer::from_slice(bytes);

                if let Some(s) = &streamer {
                    if let Err(e) = s.push(buffer.clone(), frame.id) {
                        error!("Streamer failed to process frame {}: {}", frame.id, e);
                        // Do NOT panic here. Security video pipelines must
                        // attempt to recover instantly.
                    }
                }
                if let Some(r) = &recorder {
                    // Decide whether this frame is recorded (motion/schedule
                    // gating). The live RTSP stream is never gated.
                    let motion_now = media_motion_flag.load(Ordering::Relaxed);
                    if motion_now {
                        last_motion_at = Some(Instant::now());
                    }
                    let motion_window = motion_now
                        || last_motion_at.is_some_and(|t| t.elapsed() < motion_post_roll);
                    let armed = media_schedule_armed.load(Ordering::Relaxed);
                    let record = match record_mode.as_str() {
                        "motion" => motion_window,
                        "schedule" => armed,
                        "motion_and_schedule" => armed && motion_window,
                        _ => true, // "continuous" and unknown values
                    };
                    if record {
                        if let Err(e) = r.push(buffer.clone(), frame.id) {
                            // A dead recorder pipeline cannot heal in-place;
                            // stop feeding it but keep the live stream running.
                            error!("Recorder stopped ({e}); live stream continues");
                            recorder = None;
                        }
                    }
                }
            }
        })
        .unwrap();

    // ==========================================
    // THREAD 0: THE CAPTURE LOOP (Main Thread)
    // ==========================================
    info!("Entering real-time capture loop...");

    // CPU Pinning: ONLY executes if compiled for the Linux target
    #[cfg(target_os = "linux")]
    {
        if core_affinity::set_for_current(core_affinity::CoreId { id: 1 }) {
            info!("Capture Thread strictly pinned to CPU Core 1");
        }
    }

    let mut frame_count: u64 = 0;
    let mut consecutive_drops: u32 = 0;
    let max_consecutive_drops = app_config.watchdog.max_consecutive_dropped_frames;

    // Every worker thread is spawned and the camera is streaming — systemd
    // (Type=notify) can now start counting this unit as up. No-ops off a
    // systemd host (see core/sd_notify.rs).
    core::sd_notify::ready();
    let watchdog_ping_interval = core::sd_notify::watchdog_interval();
    let mut last_watchdog_ping = Instant::now();

    // The Erlang-style Supervisor Loop
    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("Shutdown signal received; stopping capture");
            break;
        }

        match camera.dequeue_frame() {
            Ok(frame) => {
                // Pass the lock-free reference pointer to the AI and Streamer threads
                if let Err(e) = router.route_frame(frame) {
                    metrics.frames_dropped.inc();
                    consecutive_drops += 1;
                    // route_frame already rate-limits its own "queue full"
                    // warning; only echo the streak start and then a
                    // periodic heartbeat here, so a stalled consumer logs a
                    // trend instead of one line per dropped frame.
                    if consecutive_drops == 1 || consecutive_drops.is_multiple_of(100) {
                        warn!(consecutive_drops, "Router dropped frame: {}", e);
                    }
                    // A long unbroken drop streak means the consumers are
                    // wedged, not merely busy — restart into a clean state.
                    if max_consecutive_drops > 0 && consecutive_drops >= max_consecutive_drops {
                        error!(
                            consecutive_drops,
                            "CRITICAL: consumers wedged (max_consecutive_dropped_frames); restarting"
                        );
                        process::exit(2);
                    }
                } else {
                    consecutive_drops = 0;
                }

                frame_count += 1;
                metrics.frames_captured.inc();
                frames_processed.store(frame_count, Ordering::Relaxed);

                // Keep the logs quiet in production, only pulse health checks
                if frame_count.is_multiple_of(300) {
                    info!("System healthy. Processed {} frames.", frame_count);
                }
            }
            Err(e) => {
                error!("Capture fault: {}", e);
                // In a production factory, we break the loop here to trigger the systemd auto-restart
                break;
            }
        }

        // ---------------------------------------------------------
        // Watchdog / Supervisor Check
        // ---------------------------------------------------------
        // A worker that PANICKED and exited (is_finished), or one still alive
        // but WEDGED past the heartbeat timeout — either way, exit(2) so the
        // init system restarts us into a clean state.
        if ai_handle.is_finished() || stream_handle.is_finished() {
            error!("CRITICAL FAULT: A worker thread terminated unexpectedly.");
            error!("Initiating firmware reboot sequence to recover safe state.");
            process::exit(2);
        }
        if let Some((worker, stale_ms)) = watchdog.stalled_worker() {
            error!(
                worker = %worker,
                stale_ms,
                timeout_ms = watchdog.timeout().as_millis() as u64,
                "CRITICAL: worker thread heartbeat stalled; restarting"
            );
            process::exit(2);
        }
        // Only reached when nothing above already exited(2) — systemd's
        // watchdog restart is deliberately tied to this exact same
        // liveness check, never an independent "am I healthy" signal that
        // could disagree with it.
        if let Some(interval) = watchdog_ping_interval {
            if last_watchdog_ping.elapsed() >= interval {
                core::sd_notify::watchdog_ping();
                last_watchdog_ping = Instant::now();
            }
        }
    }

    // Graceful shutdown: closing the router's channels ends the worker
    // loops; the media thread then drops the recorder, whose EOS lets
    // splitmuxsink finalize the in-flight chunk's moov atom.
    drop(router);
    if ai_handle.join().is_err() {
        warn!("Analytics thread panicked during shutdown");
    }
    if stream_handle.join().is_err() {
        warn!("Media thread panicked during shutdown");
    }
    let _ = camera.stop_stream();
    info!("Firmware shutdown gracefully.");
}

/// Rate-limited event snapshot: encode the current frame to JPEG and store
/// it under <storage>/snapshots/. Returns the saved path, or None when
/// disabled, rate-limited, or failed (never blocks the analytics loop long —
/// encoding happens at event rates only).
fn capture_snapshot(
    pixels: &[u8],
    camera: &CameraConfig,
    storage_cfg: &StorageConfig,
    device_id: &str,
    reason: &str,
    last_snapshot: &mut Option<Instant>,
) -> Option<String> {
    if !storage_cfg.snapshots_enabled {
        return None;
    }
    let min_interval = Duration::from_secs(storage_cfg.snapshot_min_interval_s.max(1));
    if last_snapshot.map(|t| t.elapsed() < min_interval) == Some(true) {
        return None;
    }
    let result = media::encode_jpeg(pixels, camera, storage_cfg.snapshot_quality)
        .and_then(|jpeg| storage::save_snapshot(&storage_cfg.path, device_id, reason, &jpeg));
    match result {
        Ok(path) => {
            *last_snapshot = Some(Instant::now());
            info!(path = %path, reason = %reason, "Event snapshot saved");
            Some(path)
        }
        Err(e) => {
            warn!("Event snapshot failed: {e}");
            None
        }
    }
}
