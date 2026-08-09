// src/commands.rs
//
// MQTT command channel (F8 downlink): the platform sends JSON commands to
// `iiotedge/<group>/<node>/cmd`, the firmware acknowledges every one on
// `.../cmd/ack`. Identity and broker come from the SAME edge.toml the
// telemetry engine uses, so uplink and downlink can never disagree.
//
// The iiotedge-lib transport owns the uplink connection (and only honours
// Sparkplug Rebirth itself); commands are a firmware concern, served by a
// small dedicated subscriber connection on its own thread.
//
// v1 command set: status, snapshot, clip, config_get, config_get_ai_rules,
// config_set_ai_rules, reboot, export, stream_start, stream_stop,
// reanalyze, test_detect. Every command is audit-logged; unknown commands
// ack with ok=false.
//
// stream_start/stream_stop implement media-ingestion-service's cloud-push
// contract (see that service's own README): its RTSP-ingest side is done
// and tested, and this is the firmware half it was waiting on — a device
// that only serves RTSP on the LAN (src/stream/rtsp_server.rs, unchanged
// by this) can now be told to also relay that same feed up to MediaMTX,
// on demand, without becoming a permanent cloud-connected stream by
// default (see [cloud_relay].enabled). stream_start's `mode` field
// ("rtsp" default, or "webrtc") picks which transport — see
// src/stream/relay.rs's module header for why both exist.
use crate::config::AppConfig;
use crate::core::metrics::Metrics;
use crate::storage::clips::ClipExtractor;
use crate::storage::export::ExportTrigger;
use crate::stream::relay::{RelayTarget, StreamRelay};

use iiotedge_core::EdgeConfig;
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Shared handles the command executor acts through. Cloned once more for
/// the cluster_commands executor thread (src/main.rs) so peer-issued
/// commands run through the exact same audited `handle_command` path as
/// MQTT-issued ones — everything here is already `Arc`/`Clone`-cheap.
#[derive(Clone)]
pub struct CommandContext {
    pub device_id: String,
    pub firmware_config_path: String,
    pub metrics: Arc<Metrics>,
    pub booted: Instant,
    /// Set by the `snapshot` command, consumed by the analytics thread on
    /// the next frame.
    pub manual_snapshot: Arc<AtomicBool>,
    pub clip_extractor: Option<ClipExtractor>,
    /// Fires a full evidence sync on every export target (SD/FTP).
    pub export_trigger: Option<ExportTrigger>,
    /// Bearer token required in every command (empty = unauthenticated).
    pub command_token: String,
    /// stream_start/stream_stop: cloud-push relay to media-ingestion-service
    /// (see [cloud_relay] config; off by default).
    pub cloud_relay_enabled: bool,
    pub relay: Arc<StreamRelay>,
    /// The firmware's own local RTSP pull URL (with credentials embedded,
    /// if [security].rtsp_auth is on) — precomputed once at boot so the
    /// command handler never needs to re-derive it per call.
    pub local_rtsp_url: String,
    /// `reanalyze` command: consumed by the AI engine thread, which calls
    /// `InferenceEngine::force_next()` on the next loop iteration.
    pub reanalyze_requested: Arc<AtomicBool>,
    /// Gates the `test_detect` command (see [ai].test_hooks_enabled) — off
    /// by default, meant to stay off on any device with a real camera.
    pub ai_test_hooks_enabled: bool,
    /// `test_detect`: injects straight into the same fusion state a real
    /// detection would (see cluster/fusion.rs).
    pub fusion: Option<Arc<crate::cluster::fusion::DetectionFusion>>,
    /// `test_detect`: broadcasts the synthetic detection onto the cluster
    /// bus exactly like a genuine `ai_event` would.
    pub cluster: Option<crate::cluster::ClusterHandle>,
    /// ptz_move/ptz_stop/ptz_preset: the exact same instance the ONVIF PTZ
    /// service dispatches through (src/onvif/ptz.rs) — one PtzController
    /// per device, since it owns an exclusive serial handle.
    pub ptz: Option<Arc<crate::ptz::PtzController>>,
    /// config_get_ai_rules/config_set_ai_rules (Phase 20, src/runtime_config.rs):
    /// `ai.labels` at boot, for the same class-membership validation
    /// config.rs::validate_ai_rules already applies at boot time.
    pub ai_labels: Vec<String>,
    /// Where a remotely-set rule list persists across reboots
    /// ([system].ai_rules_override_file).
    pub ai_rules_override_path: String,
    /// Cross-thread handoff to the analytics thread's live RuleEngine — the
    /// exact same slot `POST /config/ai-rules` (src/core/metrics.rs) writes
    /// to, so both channels apply through the one running engine.
    pub rule_update_slot: crate::runtime_config::RuleUpdateSlot,
}

/// Start the command subscriber; failures log and disable commands — never
/// the camera.
///
/// Deliberately NOT registered with the firmware's shared watchdog
/// (tried 2026-08-09, reverted same day — see git history/RELEASE_NOTES
/// for the full account). Two problems, not one: (1) that watchdog uses a
/// single global timeout tuned for the analytics/media threads, which
/// heartbeat every video frame — this thread only produces an event when
/// MQTT actually has traffic, and with a 30s keepalive a healthy, idle
/// connection can easily go 15-30s between events, so it false-positived
/// as "stalled" during completely normal operation. (2) far more
/// seriously: the watchdog's response is `process::exit(2)` — an abrupt,
/// whole-process kill with no coordination with other threads. Firing
/// that while the spawned worker below (see its own doc comment) is
/// mid-flight inside a `handle_command` call that touches GStreamer (e.g.
/// building a relay pipeline for `stream_start`) segfaulted the process
/// outright (confirmed on a real device: `status=11/SEGV`, not a clean
/// exit(2)) — GStreamer's C internals don't tolerate the process
/// disappearing out from under a thread that's actively using them.
/// Real watchdog coverage for this thread needs its own independently-
/// tuned timeout and a teardown path that doesn't hard-kill through
/// in-flight FFI work — genuinely separate design work, not a quick
/// bolt-on to the existing single-timeout mechanism built for a
/// different kind of thread.
pub fn spawn(cfg: &AppConfig, ctx: CommandContext) {
    if !cfg.telemetry.enabled {
        info!("Command channel disabled (telemetry off)");
        return;
    }
    let edge_config_path = cfg.telemetry.edge_config.clone();
    let spawned = thread::Builder::new()
        .name("command_channel".to_string())
        .spawn(move || run(&edge_config_path, ctx));
    if let Err(e) = spawned {
        warn!("Failed to spawn command channel: {e}");
    }
}

fn run(edge_config_path: &str, ctx: CommandContext) {
    let edge = match EdgeConfig::from_file(edge_config_path) {
        Ok(config) => config,
        Err(e) => {
            warn!("Command channel disabled: cannot load {edge_config_path}: {e}");
            return;
        }
    };
    let mqtt = &edge.northbound.mqtt;
    let cmd_topic = format!("iiotedge/{}/{}/cmd", edge.node.group_id, edge.node.node_id);
    let ack_topic = format!("{cmd_topic}/ack");

    let mut options = MqttOptions::new(
        format!("{}-cmd", edge.node.node_id),
        mqtt.host.clone(),
        mqtt.port,
    );
    options.set_keep_alive(Duration::from_secs(30));

    // Ride the same TLS the telemetry uplink uses (edge.toml [northbound.tls]).
    crate::core::mqtt_tls::apply(&mut options, &edge.northbound.tls, "command channel");
    if edge.northbound.tls.enabled {
        info!("Command channel using TLS");
    }

    let (client, mut connection) = Client::new(options, 16);
    info!(topic = %cmd_topic, "Command channel connecting");

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                if let Err(e) = client.subscribe(&cmd_topic, QoS::AtLeastOnce) {
                    warn!("command subscribe failed: {e}");
                } else {
                    info!(topic = %cmd_topic, "Command channel ready");
                }
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                // Handling used to run inline, right here, blocking this
                // same loop for as long as `handle_command` took (GStreamer
                // relay/pipeline setup, PTZ serial I/O, file I/O — none of
                // it fast-path-bounded) — and `client.publish`'s ack send
                // blocks on rumqttc's own bounded (cap-16) request channel,
                // which only drains via this exact loop, i.e. a burst of
                // commands could self-deadlock the very thread meant to
                // service them. While blocked, nothing was reading the
                // socket or answering MQTT's keepalive PINGREQ, so a
                // connection the broker closed mid-handling (FIN — the
                // socket ends up in CLOSE_WAIT) went completely undetected:
                // `rumqttc`'s own reconnect-on-next-poll logic is correct
                // (verified against its source) but literally cannot run
                // while this thread is elsewhere. Confirmed as the live
                // cause on a real device (2026-08-09): CLOSE_WAIT on the
                // command socket, zero reconnect log, device otherwise
                // healthy, no commands processed until a manual restart.
                // Fix: hand the work to its own thread so `connection.iter()`
                // is re-entered immediately regardless of handler duration —
                // matching how iiotedge-lib's own `MqttTransport` already
                // never runs handlers inline for exactly this reason.
                let worker_client = client.clone();
                let worker_ctx = ctx.clone();
                let worker_ack_topic = ack_topic.clone();
                thread::spawn(move || {
                    let ack = handle_command(&publish.payload, &worker_ctx);
                    let reboot = ack.get("reboot").and_then(|v| v.as_bool()).unwrap_or(false);
                    if let Err(e) = worker_client.publish(
                        &worker_ack_topic,
                        QoS::AtLeastOnce,
                        false,
                        ack.to_string(),
                    ) {
                        warn!("command ack publish failed: {e}");
                    }
                    if reboot {
                        // Give the ack a moment to leave, then hand control
                        // to systemd (Restart=always brings us back up).
                        thread::sleep(Duration::from_millis(500));
                        info!("Reboot command honored; exiting for supervisor restart");
                        std::process::exit(3);
                    }
                });
            }
            Ok(_) => {}
            Err(e) => {
                warn!("command channel connection error: {e}; retrying");
                thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

/// `pub(crate)`: also called by the cluster_commands executor thread
/// (src/main.rs) to run peer-issued commands through this same audited
/// path — same validation, same ack shape, same command_token check.
pub(crate) fn handle_command(raw: &[u8], ctx: &CommandContext) -> serde_json::Value {
    let request: serde_json::Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => {
            warn!("command payload is not JSON: {e}");
            return json!({"ok": false, "error": format!("invalid JSON: {e}")});
        }
    };
    let cmd = request.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    let id = request.get("id").cloned().unwrap_or(json!(null));
    info!(cmd = %cmd, id = %id, "Command received");

    // Bearer-token auth: reject before any side effect if a token is required
    // and absent/wrong. Unauthenticated only when command_token is empty.
    if !ctx.command_token.is_empty() {
        let presented = request.get("token").and_then(|t| t.as_str()).unwrap_or("");
        if presented != ctx.command_token {
            warn!(cmd = %cmd, "command rejected: invalid or missing token");
            return json!({"ok": false, "error": "unauthorized: invalid or missing token", "id": id, "cmd": cmd});
        }
    }

    let mut ack = match cmd {
        "status" => json!({
            "ok": true,
            "device_id": ctx.device_id,
            "firmware_version": env!("CARGO_PKG_VERSION"),
            "uptime_s": ctx.booted.elapsed().as_secs(),
            "frames_captured": ctx.metrics.frames_captured.get(),
            "frames_dropped": ctx.metrics.frames_dropped.get(),
            "ai_detections": ctx.metrics.ai_detections.get(),
            "tamper_active": ctx.metrics.tamper_active.get() == 1,
        }),
        "snapshot" => {
            ctx.manual_snapshot.store(true, Ordering::Relaxed);
            json!({"ok": true, "detail": "snapshot scheduled for next frame"})
        }
        "clip" => match &ctx.clip_extractor {
            Some(clips) => {
                clips.request("manual", json!({"requested_by": "command"}));
                json!({"ok": true, "detail": "clip extraction scheduled (waits out post-roll)"})
            }
            None => json!({"ok": false, "error": "storage/clips disabled on this device"}),
        },
        "config_get" => match std::fs::read_to_string(&ctx.firmware_config_path) {
            Ok(contents) => json!({"ok": true, "config": contents}),
            Err(e) => json!({"ok": false, "error": format!("read config: {e}")}),
        },
        // Remote AI/automation config (Phase 20, src/runtime_config.rs):
        // read-modify-write pair for `[[ai.rules]]` specifically (not the
        // whole AppConfig — camera/stream/security changes still need the
        // static config file + a restart). `config_get_ai_rules` returns
        // whatever's actually in effect right now (the persisted override
        // if one exists, otherwise the static file's own rules).
        "config_get_ai_rules" => {
            let rules = crate::runtime_config::current_rules(
                &ctx.ai_rules_override_path,
                &ctx.firmware_config_path,
            );
            json!({"ok": true, "rules": rules})
        }
        // {"cmd": "config_set_ai_rules", "rules": [ ... same shape as
        // [[ai.rules]] in TOML, as JSON ... ]} — REPLACES the entire rule
        // list (not a merge/patch), same all-or-nothing semantics as
        // editing the [[ai.rules]] array in the config file by hand.
        // Validated through the exact function boot-time config loading
        // uses (config::validate_ai_rules) before anything is persisted or
        // applied — an invalid payload changes nothing.
        "config_set_ai_rules" => match request.get("rules").cloned() {
            None => json!({"ok": false, "error": "missing 'rules' array"}),
            Some(raw_rules) => {
                match serde_json::from_value::<Vec<crate::config::AiRule>>(raw_rules) {
                    Err(e) => json!({"ok": false, "error": format!("malformed rules: {e}")}),
                    Ok(rules) => match crate::runtime_config::apply_and_persist(
                        rules,
                        &ctx.ai_labels,
                        &ctx.ai_rules_override_path,
                        &ctx.firmware_config_path,
                        &ctx.rule_update_slot,
                    ) {
                        Ok(count) => json!({
                            "ok": true,
                            "detail": format!(
                                "{count} rule(s) validated, persisted, and applied — takes effect on the next analyzed frame"
                            ),
                        }),
                        Err(e) => json!({"ok": false, "error": e}),
                    },
                }
            }
        },
        "reboot" => json!({"ok": true, "detail": "restarting via supervisor", "reboot": true}),
        "export" => match &ctx.export_trigger {
            Some(trigger) => {
                trigger.fire();
                json!({"ok": true, "detail": "full evidence sync triggered on all export targets"})
            }
            None => {
                json!({"ok": false, "error": "no export targets enabled ([storage.sd]/[storage.ftp])"})
            }
        },
        // media-ingestion-service's cloud-push contract: `mode` picks the
        // relay transport ("rtsp", the default, or "webrtc" — see
        // src/stream/relay.rs's module header for why WebRTC exists
        // alongside RTSP, not instead of it). RTSP mode: the platform sends
        // publish_url with NO credentials embedded; MediaMTX's auth webhook
        // checks only the RTSP password against this device's command_token
        // (username is never checked), so the same token that already
        // authenticated this MQTT command is what gets injected here. WebRTC
        // mode: WHIP's own standard auth is an `Authorization: Bearer
        // <token>` header, so the same command_token is carried as-is, no
        // URL rewriting needed.
        "stream_start" if !ctx.cloud_relay_enabled => {
            json!({"ok": false, "error": "cloud_relay.enabled is false on this device"})
        }
        "stream_start" => match parse_stream_target(&request, &ctx.command_token) {
            Err(e) => json!({"ok": false, "error": e}),
            Ok(target) => match ctx.relay.clone().start(&ctx.local_rtsp_url, target) {
                Ok(()) => json!({"ok": true, "detail": "cloud relay started"}),
                Err(e) => json!({"ok": false, "error": format!("relay start failed: {e}")}),
            },
        },
        "stream_stop" => {
            ctx.relay.stop();
            json!({"ok": true, "detail": "cloud relay stopped"})
        }
        // Peer-triggered re-analysis (F9 cross-device AI): a neighboring
        // camera saw something worth a second look, so bypass this
        // device's own ai.inference_fps_limit for the very next frame
        // instead of waiting out the normal cadence. Reached over MQTT or
        // (more commonly) via a `[[cluster.reactions]] remote_cmd =
        // "reanalyze"` rule — see src/ai/engine.rs's `force_next()`.
        "reanalyze" => {
            ctx.reanalyze_requested.store(true, Ordering::Relaxed);
            json!({"ok": true, "detail": "immediate re-inference requested on next frame"})
        }
        // Test-only: injects a synthetic detection through the exact same
        // fusion-record + cluster-broadcast path a genuine AI detection
        // uses (see main.rs's ai_engine thread). Exists for test clusters
        // running on a camera-less mock backend, where real inference
        // never fires — see [ai].test_hooks_enabled (off by default).
        "test_detect" if !ctx.ai_test_hooks_enabled => {
            json!({"ok": false, "error": "ai.test_hooks_enabled is false on this device"})
        }
        "test_detect" => {
            let label = request
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("person")
                .to_string();
            if let Some(fusion) = &ctx.fusion {
                fusion.record_local(&label);
            }
            if let Some(bus) = &ctx.cluster {
                bus.publish_event("ai_event", label.clone(), &ctx.device_id);
            }
            info!(label = %label, "test_detect: synthetic detection injected");
            json!({"ok": true, "detail": format!("synthetic detection '{label}' injected")})
        }
        // PTZ (F1-adjacent): the same PtzController the ONVIF PTZ service
        // dispatches through (src/onvif/ptz.rs), so ONVIF and MQTT control
        // never race each other or disagree about "is it moving." pan/
        // tilt/zoom follow ONVIF's own -1.0..=1.0 convention.
        "ptz_move" => match &ctx.ptz {
            Some(ptz) => {
                let pan = request.get("pan").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let tilt = request.get("tilt").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                let zoom = request.get("zoom").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
                match ptz.continuous_move(pan, tilt, zoom) {
                    Ok(()) => json!({"ok": true, "detail": "ptz moving"}),
                    Err(e) => json!({"ok": false, "error": format!("ptz move failed: {e}")}),
                }
            }
            None => json!({"ok": false, "error": "ptz.enabled is false on this device"}),
        },
        "ptz_stop" => match &ctx.ptz {
            Some(ptz) => match ptz.stop() {
                Ok(()) => json!({"ok": true, "detail": "ptz stopped"}),
                Err(e) => json!({"ok": false, "error": format!("ptz stop failed: {e}")}),
            },
            None => json!({"ok": false, "error": "ptz.enabled is false on this device"}),
        },
        // {"cmd": "ptz_preset", "preset": 1, "mode": "set" | "goto"}
        // (mode defaults to "goto" — the more common remote-control case).
        "ptz_preset" => match &ctx.ptz {
            Some(ptz) => {
                let preset = request.get("preset").and_then(|v| v.as_u64());
                let mode = request
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("goto");
                match (preset, mode) {
                    (None, _) => json!({"ok": false, "error": "missing numeric 'preset'"}),
                    (Some(p), _) if p > u64::from(u8::MAX) => {
                        json!({"ok": false, "error": "preset must be 0-255"})
                    }
                    (Some(p), "set") => match ptz.set_preset(p as u8) {
                        Ok(()) => json!({"ok": true, "detail": format!("preset {p} set")}),
                        Err(e) => {
                            json!({"ok": false, "error": format!("ptz set-preset failed: {e}")})
                        }
                    },
                    (Some(p), "goto") => match ptz.goto_preset(p as u8) {
                        Ok(()) => json!({"ok": true, "detail": format!("moving to preset {p}")}),
                        Err(e) => {
                            json!({"ok": false, "error": format!("ptz goto-preset failed: {e}")})
                        }
                    },
                    (Some(_), other) => json!({
                        "ok": false,
                        "error": format!("unknown mode '{other}' (expected 'set' or 'goto')")
                    }),
                }
            }
            None => json!({"ok": false, "error": "ptz.enabled is false on this device"}),
        },
        other => json!({
            "ok": false,
            "error": format!("unknown command '{other}'"),
            "supported": [
                "status", "snapshot", "clip", "config_get", "config_get_ai_rules",
                "config_set_ai_rules", "export", "reboot", "stream_start",
                "stream_stop", "reanalyze", "test_detect",
                "ptz_move", "ptz_stop", "ptz_preset",
            ],
        }),
    };
    if let Some(object) = ack.as_object_mut() {
        object.insert("id".into(), id);
        object.insert("cmd".into(), json!(cmd));
    }
    ack
}

/// Rewrites a credential-less `rtsp://host:port/path` publish URL (what
/// media-ingestion-service's stream_start command actually sends) into one
/// carrying `token` as the RTSP password, matching what MediaMTX's auth
/// webhook checks on that service (password == the device's command_token;
/// the username is never validated, so it's a fixed placeholder here).
fn inject_rtsp_credentials(url: &str, token: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("rtsp://")
        .ok_or_else(|| format!("publish_url is not an rtsp:// URL: {url}"))?;
    if rest.contains('@') {
        // Unexpected today (the documented contract's publish_url never
        // carries userinfo) but don't clobber it if a future server
        // version starts sending one.
        return Ok(url.to_string());
    }
    Ok(format!("rtsp://device:{token}@{rest}"))
}

/// Parses a `stream_start` payload into the relay target it names — pure
/// and independent of any GStreamer pipeline construction (see
/// src/stream/relay.rs), so it's directly unit-testable. `mode` defaults
/// to `"rtsp"` for backward compatibility with callers that predate
/// WebRTC support.
fn parse_stream_target(
    request: &serde_json::Value,
    command_token: &str,
) -> Result<RelayTarget, String> {
    let mode = request
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("rtsp");
    match mode {
        "rtsp" => match request.get("publish_url").and_then(|v| v.as_str()) {
            None | Some("") => Err("missing publish_url".to_string()),
            Some(publish_url) => {
                let publish_url = inject_rtsp_credentials(publish_url, command_token)?;
                Ok(RelayTarget::Rtsp { publish_url })
            }
        },
        "webrtc" => match request.get("whip_url").and_then(|v| v.as_str()) {
            None | Some("") => Err("missing whip_url".to_string()),
            Some(whip_url) => Ok(RelayTarget::Webrtc {
                whip_url: whip_url.to_string(),
                auth_token: command_token.to_string(),
            }),
        },
        other => Err(format!(
            "unknown mode \"{other}\" (expected \"rtsp\" or \"webrtc\")"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_token_as_rtsp_password() {
        let url = inject_rtsp_credentials("rtsp://media.example.com:8554/cam-1", "secret-tok")
            .expect("valid rtsp url");
        assert_eq!(url, "rtsp://device:secret-tok@media.example.com:8554/cam-1");
    }

    #[test]
    fn rejects_non_rtsp_url() {
        assert!(inject_rtsp_credentials("http://example.com", "tok").is_err());
    }

    #[test]
    fn leaves_existing_credentials_untouched() {
        let url =
            inject_rtsp_credentials("rtsp://user:pass@host:8554/x", "tok").expect("valid rtsp url");
        assert_eq!(url, "rtsp://user:pass@host:8554/x");
    }

    #[test]
    fn stream_target_defaults_to_rtsp_mode_when_mode_is_omitted() {
        let request =
            json!({"cmd": "stream_start", "publish_url": "rtsp://media.example.com/cam-1"});
        match parse_stream_target(&request, "tok").expect("valid request") {
            RelayTarget::Rtsp { publish_url } => {
                assert_eq!(publish_url, "rtsp://device:tok@media.example.com/cam-1");
            }
            RelayTarget::Webrtc { .. } => panic!("expected rtsp, got webrtc"),
        }
    }

    #[test]
    fn stream_target_rtsp_mode_requires_publish_url() {
        let request = json!({"cmd": "stream_start", "mode": "rtsp"});
        let err = parse_stream_target(&request, "tok").expect_err("missing publish_url");
        assert!(err.contains("publish_url"));
    }

    #[test]
    fn stream_target_webrtc_mode_carries_command_token_as_bearer_auth() {
        let request = json!({
            "cmd": "stream_start",
            "mode": "webrtc",
            "whip_url": "https://media.example.com/whip/cam-1",
        });
        match parse_stream_target(&request, "secret-tok").expect("valid request") {
            RelayTarget::Webrtc {
                whip_url,
                auth_token,
            } => {
                assert_eq!(whip_url, "https://media.example.com/whip/cam-1");
                assert_eq!(auth_token, "secret-tok");
            }
            RelayTarget::Rtsp { .. } => panic!("expected webrtc, got rtsp"),
        }
    }

    #[test]
    fn stream_target_webrtc_mode_requires_whip_url() {
        let request = json!({"cmd": "stream_start", "mode": "webrtc"});
        let err = parse_stream_target(&request, "tok").expect_err("missing whip_url");
        assert!(err.contains("whip_url"));
    }

    #[test]
    fn stream_target_rejects_an_unknown_mode() {
        let request = json!({"cmd": "stream_start", "mode": "rtmp"});
        let err = parse_stream_target(&request, "tok").expect_err("unknown mode");
        assert!(err.contains("rtmp"));
    }
}
