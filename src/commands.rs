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
// v1 command set: status, snapshot, clip, config_get, reboot, export,
// stream_start, stream_stop, reanalyze, test_detect. Every command is
// audit-logged; unknown commands ack with ok=false.
//
// stream_start/stream_stop implement media-ingestion-service's cloud-push
// contract (see that service's own README): its RTSP-ingest side is done
// and tested, and this is the firmware half it was waiting on — a device
// that only serves RTSP on the LAN (src/stream/rtsp_server.rs, unchanged
// by this) can now be told to also relay that same feed up to MediaMTX,
// on demand, without becoming a permanent cloud-connected stream by
// default (see [cloud_relay].enabled).
use crate::config::AppConfig;
use crate::core::metrics::Metrics;
use crate::storage::clips::ClipExtractor;
use crate::storage::export::ExportTrigger;
use crate::stream::relay::StreamRelay;

use iiotedge_core::EdgeConfig;
use rumqttc::{Client, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
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
}

/// Start the command subscriber; failures log and disable commands — never
/// the camera.
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
    // Raw-PEM Simple config avoids coupling to any rustls version.
    let tls = &edge.northbound.tls;
    if tls.enabled {
        match std::fs::read(&tls.ca_path) {
            Ok(ca) => {
                let client_auth = match (
                    tls.client_cert_path.is_empty(),
                    tls.client_key_path.is_empty(),
                ) {
                    (false, false) => {
                        match (
                            std::fs::read(&tls.client_cert_path),
                            std::fs::read(&tls.client_key_path),
                        ) {
                            (Ok(cert), Ok(key)) => Some((cert, key)),
                            _ => {
                                warn!("command channel: failed to read client cert/key; using server-auth TLS only");
                                None
                            }
                        }
                    }
                    _ => None,
                };
                options.set_transport(Transport::Tls(TlsConfiguration::Simple {
                    ca,
                    alpn: None,
                    client_auth,
                }));
                info!("Command channel using TLS");
            }
            Err(e) => warn!(
                "command channel: cannot read TLS ca_path '{}': {e}; connecting plaintext",
                tls.ca_path
            ),
        }
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
                let ack = handle_command(&publish.payload, &ctx);
                let reboot = ack.get("reboot").and_then(|v| v.as_bool()).unwrap_or(false);
                if let Err(e) = client.publish(&ack_topic, QoS::AtLeastOnce, false, ack.to_string())
                {
                    warn!("command ack publish failed: {e}");
                }
                if reboot {
                    // Give the ack a moment to leave, then hand control to
                    // systemd (Restart=always brings us back up).
                    thread::sleep(Duration::from_millis(500));
                    info!("Reboot command honored; exiting for supervisor restart");
                    std::process::exit(3);
                }
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
        // media-ingestion-service's cloud-push contract: the platform sends
        // publish_url with NO credentials embedded; MediaMTX's auth webhook
        // checks only the RTSP password against this device's command_token
        // (username is never checked), so the same token that already
        // authenticated this MQTT command is what gets injected here.
        "stream_start" if !ctx.cloud_relay_enabled => {
            json!({"ok": false, "error": "cloud_relay.enabled is false on this device"})
        }
        "stream_start" => match request.get("publish_url").and_then(|v| v.as_str()) {
            None | Some("") => json!({"ok": false, "error": "missing publish_url"}),
            Some(publish_url) => match inject_rtsp_credentials(publish_url, &ctx.command_token) {
                Ok(authed_url) => match ctx.relay.clone().start(&ctx.local_rtsp_url, &authed_url) {
                    Ok(()) => json!({"ok": true, "detail": "cloud relay started"}),
                    Err(e) => json!({"ok": false, "error": format!("relay start failed: {e}")}),
                },
                Err(e) => json!({"ok": false, "error": e}),
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
        other => json!({
            "ok": false,
            "error": format!("unknown command '{other}'"),
            "supported": [
                "status", "snapshot", "clip", "config_get", "export", "reboot",
                "stream_start", "stream_stop", "reanalyze", "test_detect",
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
}
