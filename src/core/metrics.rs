// src/core/metrics.rs
//
// Prometheus metrics + health endpoint. Served over HTTP on
// [system].metrics_port:
//   /metrics            — Prometheus text exposition (scrape target)
//   /healthz            — liveness JSON (load balancers / systemd watchdog scripts)
//   /cluster/status     — mesh topology snapshot for the mobile app's Cluster
//                         tab (see [cluster] config); {"enabled":false} when
//                         cluster mode is off, same route either way so
//                         clients don't need to special-case it. Gated by
//                         [security].api_token when set (Bearer header).
//   /onboarding/info    — JSON payload for QR device onboarding (Phase 12,
//                         see src/onboarding.rs). Gated by
//                         [security].command_token when set.
//   /onboarding/qr.png  — the same payload rendered as a scannable PNG.
//   /footprint          — device footprint (Phase 12, F10): model, exact
//                         firmware commit, config hash, enabled features.
//                         See src/footprint.rs.
//   /config/ai-rules    — GET returns the AI/automation rules actually in
//                         effect right now; POST {"rules":[...]} replaces
//                         them (Phase 20, src/runtime_config.rs). Same
//                         capability as the MQTT config_get_ai_rules/
//                         config_set_ai_rules commands, for anything that
//                         isn't an MQTT client. Gated by
//                         [security].command_token when set.
//
// Counters are cheap atomics — hot paths (capture loop, analytics) update
// them without locks.
use crate::cluster::ClusterHandle;
use crate::footprint::Footprint;
use crate::onboarding::OnboardingContext;
use crate::runtime_config::RuleUpdateSlot;
use prometheus::{Encoder, IntCounter, IntGauge, Registry, TextEncoder};
use serde_json::json;
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;
use tiny_http::{Header, Method, Request, Response, Server};
use tracing::{info, warn};

/// Hard cap on `POST /config/ai-rules`'s request body, independent of
/// `Content-Length` (never trusted alone — see that route's own comment).
/// Generous for any realistic rule set (`config::MAX_AI_RULES` rules at
/// `config::MAX_ZONE_POINTS` points each, comfortably fits) while bounding
/// how much a single request can cost this single-threaded server.
const MAX_AI_RULES_BODY_BYTES: usize = 256 * 1024;

/// Bearer-token-gated read/write access to `[[ai.rules]]` over HTTP (Phase
/// 20) — the same capability the MQTT `config_get_ai_rules`/
/// `config_set_ai_rules` commands expose (src/commands.rs), for anything
/// that isn't an MQTT client (curl, a browser, a simple HTTP integration).
/// Both channels funnel through `runtime_config::apply_and_persist`, so
/// neither can accept something the other would reject.
pub struct RuntimeConfigContext {
    pub ai_labels: Vec<String>,
    pub ai_rules_override_path: String,
    pub static_config_path: String,
    pub rule_update_slot: RuleUpdateSlot,
    pub command_token: String,
}

pub struct Metrics {
    registry: Registry,
    pub frames_captured: IntCounter,
    pub frames_dropped: IntCounter,
    pub ai_detections: IntCounter,
    pub tamper_alarms: IntCounter,
    pub tamper_active: IntGauge,
    pub uptime_seconds: IntGauge,
    pub files_exported: IntCounter,
    pub storage_used_mb: IntGauge,
    pub storage_free_mb: IntGauge,
    pub soc_temp_millicelsius: IntGauge,
    pub cpu_load_percent: IntGauge,
    pub mem_used_percent: IntGauge,
    pub throttled: IntGauge,
    pub cluster_peer_count: IntGauge,
    pub cluster_is_leader: IntGauge,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        let registry = Registry::new();
        let frames_captured = IntCounter::new(
            "iiotedge_frames_captured_total",
            "Frames captured from the sensor",
        )
        .expect("valid metric");
        let frames_dropped = IntCounter::new(
            "iiotedge_frames_dropped_total",
            "Frames dropped by the bounded router (backpressure)",
        )
        .expect("valid metric");
        let ai_detections =
            IntCounter::new("iiotedge_ai_detections_total", "AI detections emitted")
                .expect("valid metric");
        let tamper_alarms =
            IntCounter::new("iiotedge_tamper_alarms_total", "Tamper alarm activations")
                .expect("valid metric");
        let tamper_active = IntGauge::new(
            "iiotedge_tamper_active",
            "1 while any tamper condition is alarmed",
        )
        .expect("valid metric");
        let uptime_seconds =
            IntGauge::new("iiotedge_uptime_seconds", "Firmware uptime").expect("valid metric");
        let files_exported = IntCounter::new(
            "iiotedge_files_exported_total",
            "Evidence files exported to SD/FTP targets",
        )
        .expect("valid metric");
        let storage_used_mb = IntGauge::new(
            "iiotedge_storage_used_mb",
            "Recording chunk bytes on disk (MiB)",
        )
        .expect("valid metric");
        let storage_free_mb = IntGauge::new(
            "iiotedge_storage_free_mb",
            "Free space on the storage filesystem (MiB)",
        )
        .expect("valid metric");
        let soc_temp_millicelsius = IntGauge::new(
            "iiotedge_soc_temp_millicelsius",
            "Hottest SoC thermal zone (milli-degrees C)",
        )
        .expect("valid metric");
        let cpu_load_percent =
            IntGauge::new("iiotedge_cpu_load_percent", "CPU busy percent").expect("valid metric");
        let mem_used_percent = IntGauge::new("iiotedge_mem_used_percent", "Used memory percent")
            .expect("valid metric");
        let throttled = IntGauge::new(
            "iiotedge_thermal_throttled",
            "1 while thermal throttling is active",
        )
        .expect("valid metric");
        let cluster_peer_count = IntGauge::new(
            "iiotedge_cluster_peer_count",
            "Cluster peers currently seen (excludes self)",
        )
        .expect("valid metric");
        let cluster_is_leader = IntGauge::new(
            "iiotedge_cluster_is_leader",
            "1 while this node holds the cluster coordinator role",
        )
        .expect("valid metric");

        for collector in [
            Box::new(frames_captured.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(frames_dropped.clone()),
            Box::new(ai_detections.clone()),
            Box::new(tamper_alarms.clone()),
            Box::new(tamper_active.clone()),
            Box::new(uptime_seconds.clone()),
            Box::new(files_exported.clone()),
            Box::new(storage_used_mb.clone()),
            Box::new(storage_free_mb.clone()),
            Box::new(soc_temp_millicelsius.clone()),
            Box::new(cpu_load_percent.clone()),
            Box::new(mem_used_percent.clone()),
            Box::new(throttled.clone()),
            Box::new(cluster_peer_count.clone()),
            Box::new(cluster_is_leader.clone()),
        ] {
            registry.register(collector).expect("unique metric names");
        }

        Arc::new(Self {
            registry,
            frames_captured,
            frames_dropped,
            ai_detections,
            tamper_alarms,
            tamper_active,
            uptime_seconds,
            files_exported,
            storage_used_mb,
            storage_free_mb,
            soc_temp_millicelsius,
            cpu_load_percent,
            mem_used_percent,
            throttled,
            cluster_peer_count,
            cluster_is_leader,
        })
    }

    fn render(&self) -> String {
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        if encoder
            .encode(&self.registry.gather(), &mut buffer)
            .is_err()
        {
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }
}

/// Serve /metrics, /healthz, /cluster/status and (Phase 12) the QR
/// onboarding routes. Failures are logged, never fatal — a camera that
/// can't be scraped must still stream.
// This was already at clippy's 7-argument threshold before `runtime_config`
// (Phase 20) added an 8th — every parameter here is a genuinely independent
// resource this HTTP server's routes need (metrics, timing, cluster/
// onboarding/footprint/runtime-config contexts), not a group that wants to
// be its own struct; bundling the unrelated pre-existing ones together
// just to satisfy the lint would be a bigger, unrelated refactor.
#[allow(clippy::too_many_arguments)]
pub fn spawn_server(
    metrics: Arc<Metrics>,
    port: u16,
    booted: Instant,
    version: &'static str,
    cluster: Option<ClusterHandle>,
    onboarding: Arc<OnboardingContext>,
    footprint: Arc<Footprint>,
    runtime_config: RuntimeConfigContext,
) {
    let spawned = std::thread::Builder::new()
        .name("metrics_http".to_string())
        .spawn(move || {
            let server = match Server::http(("0.0.0.0", port)) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Metrics endpoint disabled: cannot bind port {port}: {e}");
                    return;
                }
            };
            info!("Metrics at http://0.0.0.0:{port}/metrics — health at /healthz");

            let text_plain =
                Header::from_bytes(&b"Content-Type"[..], &b"text/plain; version=0.0.4"[..])
                    .expect("static header");
            let app_json = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header");
            let image_png =
                Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).expect("static header");

            for mut request in server.incoming_requests() {
                metrics
                    .uptime_seconds
                    .set(booted.elapsed().as_secs() as i64);
                let full_url = request.url().to_string();
                let path = full_url.split('?').next().unwrap_or("");
                let method = request.method().clone();
                let response = match path {
                    "/metrics" => {
                        Response::from_string(metrics.render()).with_header(text_plain.clone())
                    }
                    "/healthz" | "/health" => Response::from_string(format!(
                        "{{\"status\":\"ok\",\"uptime_s\":{},\"version\":\"{}\"}}",
                        booted.elapsed().as_secs(),
                        version
                    ))
                    .with_header(app_json.clone()),
                    // Device footprint (Phase 12, F10) — model, exact
                    // firmware commit, config hash, enabled features.
                    // Unauthenticated like /healthz: nothing here is a
                    // credential (unlike /onboarding/info, which is gated).
                    "/footprint" => Response::from_string(
                        serde_json::to_string(footprint.as_ref()).unwrap_or_default(),
                    )
                    .with_header(app_json.clone()),
                    "/cluster/status" => {
                        if onboarding.api_token.is_empty()
                            || bearer_token(&request).as_deref()
                                == Some(onboarding.api_token.as_str())
                        {
                            Response::from_string(cluster_status_body(&cluster))
                                .with_header(app_json.clone())
                        } else {
                            warn!("/cluster/status rejected: missing or invalid bearer token");
                            Response::from_string(crate::onboarding::error_json(
                                "unauthorized: missing or invalid bearer token",
                            ))
                            .with_header(app_json.clone())
                            .with_status_code(401)
                        }
                    }
                    "/onboarding/info" if !onboarding.enabled => {
                        Response::from_string(crate::onboarding::error_json(
                            "onboarding disabled ([onboarding].enabled = false)",
                        ))
                        .with_header(app_json.clone())
                        .with_status_code(404)
                    }
                    "/onboarding/info" => {
                        let presented = bearer_token(&request)
                            .or_else(|| query_param(&full_url, "token"))
                            .unwrap_or_default();
                        if onboarding.authorized(&presented) {
                            let payload = onboarding.payload(&host_ip(&request));
                            Response::from_string(
                                serde_json::to_string(&payload).unwrap_or_default(),
                            )
                            .with_header(app_json.clone())
                        } else {
                            warn!("/onboarding/info rejected: missing or invalid command_token");
                            Response::from_string(crate::onboarding::error_json(
                                "unauthorized: missing or invalid token",
                            ))
                            .with_header(app_json.clone())
                            .with_status_code(401)
                        }
                    }
                    "/onboarding/qr.png" if !onboarding.enabled => {
                        Response::from_string(crate::onboarding::error_json(
                            "onboarding disabled ([onboarding].enabled = false)",
                        ))
                        .with_header(app_json.clone())
                        .with_status_code(404)
                    }
                    "/onboarding/qr.png" => {
                        let presented = bearer_token(&request)
                            .or_else(|| query_param(&full_url, "token"))
                            .unwrap_or_default();
                        if onboarding.authorized(&presented) {
                            let payload = onboarding.payload(&host_ip(&request));
                            match crate::onboarding::render_qr_png(&payload) {
                                Ok(png) => Response::from_data(png).with_header(image_png.clone()),
                                Err(e) => {
                                    warn!("QR render failed: {e}");
                                    Response::from_string(crate::onboarding::error_json(&e))
                                        .with_header(app_json.clone())
                                        .with_status_code(500)
                                }
                            }
                        } else {
                            warn!("/onboarding/qr.png rejected: missing or invalid command_token");
                            Response::from_string(crate::onboarding::error_json(
                                "unauthorized: missing or invalid token",
                            ))
                            .with_header(app_json.clone())
                            .with_status_code(401)
                        }
                    }
                    // Remote AI/automation config (Phase 20,
                    // src/runtime_config.rs) — same bearer-token gate as
                    // every other command-capable surface (empty
                    // command_token = open, same "empty means open"
                    // convention every auth knob in this firmware uses).
                    "/config/ai-rules"
                        if method == Method::Get
                            && !runtime_config.command_token.is_empty()
                            && bearer_token(&request).as_deref()
                                != Some(runtime_config.command_token.as_str()) =>
                    {
                        warn!("GET /config/ai-rules rejected: missing or invalid bearer token");
                        Response::from_string(crate::onboarding::error_json(
                            "unauthorized: missing or invalid bearer token",
                        ))
                        .with_header(app_json.clone())
                        .with_status_code(401)
                    }
                    "/config/ai-rules" if method == Method::Get => {
                        let rules = crate::runtime_config::current_rules(
                            &runtime_config.ai_rules_override_path,
                            &runtime_config.static_config_path,
                        );
                        Response::from_string(serde_json::to_string(&rules).unwrap_or_default())
                            .with_header(app_json.clone())
                    }
                    "/config/ai-rules"
                        if method == Method::Post
                            && !runtime_config.command_token.is_empty()
                            && bearer_token(&request).as_deref()
                                != Some(runtime_config.command_token.as_str()) =>
                    {
                        warn!("POST /config/ai-rules rejected: missing or invalid bearer token");
                        Response::from_string(crate::onboarding::error_json(
                            "unauthorized: missing or invalid bearer token",
                        ))
                        .with_header(app_json.clone())
                        .with_status_code(401)
                    }
                    // {"rules": [ ... same shape as [[ai.rules]] in TOML,
                    // as JSON ... ]} — REPLACES the entire rule list.
                    // Validated through config::validate_ai_rules (the
                    // exact function boot-time config loading uses) before
                    // anything is persisted or applied. Body size is capped
                    // independent of Content-Length (a client can omit or
                    // lie about that header) — this is the ONE route on
                    // this single-threaded server that reads a
                    // client-controlled body at all, and it's reachable
                    // unauthenticated whenever [security].command_token is
                    // left empty (an existing, documented "open" mode), so
                    // an unbounded read here would let one oversized/slow
                    // request stall every other route (/metrics, /healthz,
                    // /onboarding/*, ...) on this thread.
                    "/config/ai-rules" if method == Method::Post => {
                        let mut body = String::new();
                        let read_result = request
                            .as_reader()
                            .take(MAX_AI_RULES_BODY_BYTES as u64 + 1)
                            .read_to_string(&mut body);
                        if let Err(e) = read_result {
                            warn!("POST /config/ai-rules: failed to read body: {e}");
                            Response::from_string(crate::onboarding::error_json(&format!(
                                "failed to read request body: {e}"
                            )))
                            .with_header(app_json.clone())
                            .with_status_code(400)
                        } else if body.len() > MAX_AI_RULES_BODY_BYTES {
                            warn!(
                                "POST /config/ai-rules rejected: body exceeds \
                                 {MAX_AI_RULES_BODY_BYTES}-byte limit"
                            );
                            Response::from_string(crate::onboarding::error_json(&format!(
                                "request body exceeds the {MAX_AI_RULES_BODY_BYTES}-byte limit"
                            )))
                            .with_header(app_json.clone())
                            .with_status_code(413)
                        } else {
                            match parse_rules_body(&body) {
                                Err(e) => Response::from_string(crate::onboarding::error_json(&e))
                                    .with_header(app_json.clone())
                                    .with_status_code(400),
                                Ok(rules) => match crate::runtime_config::apply_and_persist(
                                    rules,
                                    &runtime_config.ai_labels,
                                    &runtime_config.ai_rules_override_path,
                                    &runtime_config.static_config_path,
                                    &runtime_config.rule_update_slot,
                                ) {
                                    Ok(count) => Response::from_string(
                                        json!({"ok": true, "applied": count}).to_string(),
                                    )
                                    .with_header(app_json.clone()),
                                    Err(e) => {
                                        warn!("POST /config/ai-rules rejected: {e}");
                                        Response::from_string(crate::onboarding::error_json(&e))
                                            .with_header(app_json.clone())
                                            .with_status_code(422)
                                    }
                                },
                            }
                        }
                    }
                    _ => Response::from_string("not found").with_status_code(404),
                };
                if let Err(e) = request.respond(response) {
                    warn!("metrics response failed: {e}");
                }
            }
        });
    if let Err(e) = spawned {
        warn!("Failed to spawn metrics server: {e}");
    }
}

fn cluster_status_body(cluster: &Option<ClusterHandle>) -> String {
    match cluster {
        Some(cluster) => {
            let view = cluster.view();
            let peers: Vec<_> = view
                .peers()
                .into_iter()
                .map(|p| {
                    json!({
                        "node_id": p.node_id,
                        "priority": p.priority,
                        "healthy": p.healthy,
                        "last_seen_ms_ago": p.last_seen_ms_ago,
                    })
                })
                .collect();
            // Best-effort observability, not a durable log: draining here
            // means acks not yet scraped by the time the next one arrives
            // are lost, which is fine for a status snapshot (poll more
            // often if that matters).
            let recent_acks: Vec<_> = cluster
                .drain_command_acks()
                .into_iter()
                .map(|a| {
                    json!({
                        "source_device": a.source_device,
                        "ack": a.ack,
                    })
                })
                .collect();
            json!({
                "enabled": true,
                "is_leader": view.is_leader(),
                "peer_count": view.peer_count(),
                "peers": peers,
                "recent_command_acks": recent_acks,
            })
            .to_string()
        }
        None => json!({"enabled": false}).to_string(),
    }
}

/// `Authorization: Bearer <token>` header value, if present.
fn bearer_token(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
}

/// Parses `POST /config/ai-rules`'s `{"rules": [...]}` body into a
/// `Vec<AiRule>`, or a client-facing error string on anything malformed
/// (invalid JSON, missing field, a rule that doesn't match the schema) —
/// separate from `runtime_config::validate_ai_rules`'s semantic checks
/// (bad mode, out-of-range zone, ...), which run afterward.
fn parse_rules_body(body: &str) -> Result<Vec<crate::config::AiRule>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let rules = value
        .get("rules")
        .ok_or_else(|| "missing 'rules' array".to_string())?;
    serde_json::from_value(rules.clone()).map_err(|e| format!("malformed rules: {e}"))
}

/// Value of `key` in a request URL's query string (`path?key=value&...`).
/// Exists so `/onboarding/qr.png?token=...` works from a plain `<img src>`
/// or a browser address bar, which can't set an Authorization header.
fn query_param(full_url: &str, key: &str) -> Option<String> {
    let (_, query) = full_url.split_once('?')?;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// LAN address the client actually reached us on (Host header — survives
/// NAT and multi-homed devices, same reasoning as onvif/services.rs), or a
/// best-effort local-route guess when no Host header is present.
fn host_ip(request: &Request) -> String {
    let host = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_else(crate::onvif::local_ip);
    host.split(':').next().unwrap_or(&host).to_string()
}
