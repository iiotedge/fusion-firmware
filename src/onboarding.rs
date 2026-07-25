// src/onboarding.rs
//
// Phase 12: QR-code device onboarding. Replaces manual IP/port/credential
// entry in the mobile app with a single scan — the QR (and its JSON
// equivalent) carries everything the app needs to add this camera:
// device identity, LAN address, ONVIF/RTSP/metrics ports, the RTSP+ONVIF
// credentials (shared [security].users store) and the `api_token` the app
// should present on subsequent `/cluster/status` calls.
//
// Two HTTP GET routes, served alongside /metrics by core/metrics.rs:
//   /onboarding/info    — JSON payload
//   /onboarding/qr.png  — the same payload rendered as a scannable PNG
//
// Both are gated by [security].command_token (empty ⇒ open, same
// "empty means unauthenticated" convention as every other auth knob in this
// firmware — see config.rs's header comment and security.rs's log_posture).
// The token can arrive either as `Authorization: Bearer <token>` (scripted
// provisioning) or `?token=<token>` (so an installer can open the QR image
// directly in a browser — an <img> tag can't set headers). This is a
// deliberately different trust tier from `api_token`: command_token is
// known to installers/provisioning tooling and never leaves that circle;
// api_token is what gets handed to the end-user's phone via the QR itself.
use crate::config::AppConfig;
use iiotedge_core::EdgeConfig;
use serde::Serialize;
use serde_json::json;
use tracing::warn;

pub struct OnboardingContext {
    pub enabled: bool,
    pub device_id: String,
    pub facility_id: String,
    pub manufacturer: String,
    pub model: String,
    pub onvif_port: u16,
    pub rtsp_port: u16,
    pub rtsp_path: String,
    pub metrics_port: u16,
    /// (username, password) from `[security].users[0]` — the same store
    /// RTSP and ONVIF already authenticate against. `None` when that list
    /// is empty (anonymous RTSP/ONVIF access).
    pub credentials: Option<(String, String)>,
    /// Embedded in the payload as the app's future `/cluster/status` bearer
    /// token. Empty means that endpoint is unauthenticated on this device.
    pub api_token: String,
    /// Gates these two routes themselves. Empty means the routes are open.
    pub command_token: String,
    /// Sparkplug group/node identity from `edge.toml` (`EdgeConfig.node`) —
    /// distinct from `device_id`/`facility_id` above (`[system]` in THIS
    /// crate's own AppConfig), a real and easily-confused gap: the mobile
    /// app previously had no way to learn this and had to guess
    /// group_id/node_id from facility_id/device_id, which are unrelated
    /// identifiers that merely happen to often look similar in test setups.
    pub group_id: String,
    pub node_id: String,
    /// The *actual* MQTT broker this device's telemetry/command channel
    /// connects to (`edge.toml` [northbound.mqtt]) — genuinely NOT derivable
    /// from this device's own LAN host: on a real deployment the broker is
    /// commonly a separate cloud/on-prem host entirely (confirmed on a real
    /// field deployment where the camera and broker were on different
    /// hosts). `None` when `[telemetry].enabled` is false, edge.toml
    /// couldn't be read, or `[northbound].transport` isn't `"mqtt"` — the
    /// app should treat MQTT wiring as "not available from this QR" in any
    /// of those cases rather than guess, same principle as the RTSP/ONVIF
    /// credentials fields already do for "no users configured".
    pub mqtt: Option<OnboardingMqtt>,
}

#[derive(Clone)]
pub struct OnboardingMqtt {
    pub host: String,
    pub port: u16,
    pub tls_enabled: bool,
}

impl OnboardingContext {
    pub fn from_config(cfg: &AppConfig) -> Self {
        let credentials = cfg
            .security
            .users
            .first()
            .map(|u| (u.username.clone(), u.password.clone()));

        let (group_id, node_id, mqtt) = if cfg.telemetry.enabled {
            match EdgeConfig::from_file(&cfg.telemetry.edge_config) {
                Ok(edge) => {
                    let mqtt = (edge.northbound.transport == "mqtt").then(|| OnboardingMqtt {
                        host: edge.northbound.mqtt.host.clone(),
                        port: edge.northbound.mqtt.port,
                        tls_enabled: edge.northbound.tls.enabled,
                    });
                    (edge.node.group_id, edge.node.node_id, mqtt)
                }
                Err(e) => {
                    warn!(
                        path = %cfg.telemetry.edge_config,
                        "onboarding: could not read edge.toml for MQTT/node identity ({e}) — \
                         QR payload will omit them",
                    );
                    (String::new(), String::new(), None)
                }
            }
        } else {
            (String::new(), String::new(), None)
        };

        Self {
            enabled: cfg.onboarding.enabled,
            device_id: cfg.system.device_id.clone(),
            facility_id: cfg.system.facility_id.clone(),
            manufacturer: cfg.onvif.manufacturer.clone(),
            model: cfg.onvif.model.clone(),
            // `external_*` overrides exist for exactly this: a device whose
            // real listen port isn't what a client outside the local
            // network namespace needs to dial (port-forwarding/NAT, or this
            // project's own Docker test cluster — see config.rs's doc
            // comment on OnboardingConfig).
            onvif_port: cfg.onboarding.external_onvif_port.unwrap_or(cfg.onvif.port),
            rtsp_port: cfg
                .onboarding
                .external_rtsp_port
                .unwrap_or(cfg.stream.rtsp_port),
            rtsp_path: cfg.stream.rtsp_path.clone(),
            metrics_port: cfg
                .onboarding
                .external_metrics_port
                .unwrap_or(cfg.system.metrics_port),
            credentials,
            api_token: cfg.security.api_token.clone(),
            command_token: cfg.security.command_token.clone(),
            group_id,
            node_id,
            mqtt,
        }
    }

    /// True when `presented` (either the bearer header value or the `token`
    /// query param, caller's choice) satisfies this device's onboarding
    /// gate. Always true when `command_token` is empty.
    pub fn authorized(&self, presented: &str) -> bool {
        self.command_token.is_empty() || presented == self.command_token
    }
}

#[derive(Serialize)]
pub struct OnboardingPayload {
    /// Bump if the shape changes incompatibly; the mobile app should reject
    /// schemas it doesn't recognize rather than guess field meanings.
    pub schema: &'static str,
    pub device_id: String,
    pub facility_id: String,
    pub manufacturer: String,
    pub model: String,
    pub firmware_version: &'static str,
    /// LAN address the phone should connect to — the same Host the request
    /// arrived on (see metrics.rs), so it's correct even on multi-homed
    /// devices, not a best-effort local-route guess.
    pub host: String,
    pub ports: OnboardingPorts,
    pub rtsp: OnboardingRtsp,
    /// ONVIF WS-UsernameToken credentials — identical to `rtsp.username`/
    /// `password` today (one shared user store), kept as its own field so a
    /// future divergence doesn't require a schema bump.
    pub onvif: OnboardingOnvif,
    /// Bearer token for `/cluster/status` and any future authenticated HTTP
    /// route. Empty string ⇒ that API is unauthenticated on this device.
    pub api_token: String,
    /// Sparkplug group/node identity (`edge.toml` [node]) — NOT the same as
    /// `device_id`/`facility_id` above, a real gap found in testing: those
    /// come from this crate's own [system] config, group_id/node_id from a
    /// separate document (iiotedge-lib's EdgeConfig). Empty strings when
    /// edge.toml couldn't be read.
    pub group_id: String,
    pub node_id: String,
    /// The actual MQTT broker this device's telemetry/command channel uses
    /// (`edge.toml` [northbound.mqtt]) — commonly a different host entirely
    /// from `host` above (a cloud/on-prem broker, not the camera itself).
    /// `null` when telemetry is off, edge.toml couldn't be read, or the
    /// configured northbound transport isn't MQTT — the app should treat
    /// that as "not available", not fall back to guessing.
    pub mqtt: Option<OnboardingMqttInfo>,
}

#[derive(Serialize)]
pub struct OnboardingMqttInfo {
    pub host: String,
    pub port: u16,
    pub tls_enabled: bool,
}

#[derive(Serialize)]
pub struct OnboardingPorts {
    pub onvif: u16,
    pub rtsp: u16,
    pub metrics: u16,
}

#[derive(Serialize)]
pub struct OnboardingRtsp {
    pub path: String,
    pub auth_required: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Fully composed convenience URL — credentials embedded exactly as
    /// `rtsp_server.rs`/`commands.rs` already do for the local pull URL.
    pub url: String,
}

#[derive(Serialize)]
pub struct OnboardingOnvif {
    pub xaddr: String,
    pub auth_required: bool,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl OnboardingContext {
    pub fn payload(&self, host_ip: &str) -> OnboardingPayload {
        let (username, password) = match &self.credentials {
            Some((u, p)) => (Some(u.clone()), Some(p.clone())),
            None => (None, None),
        };
        let auth_required = self.credentials.is_some();
        let rtsp_url = match &self.credentials {
            Some((u, p)) => format!(
                "rtsp://{u}:{p}@{host_ip}:{}{}",
                self.rtsp_port, self.rtsp_path
            ),
            None => format!("rtsp://{host_ip}:{}{}", self.rtsp_port, self.rtsp_path),
        };
        OnboardingPayload {
            schema: "iiotedge.onboarding.v1",
            device_id: self.device_id.clone(),
            facility_id: self.facility_id.clone(),
            manufacturer: self.manufacturer.clone(),
            model: self.model.clone(),
            firmware_version: env!("CARGO_PKG_VERSION"),
            host: host_ip.to_string(),
            ports: OnboardingPorts {
                onvif: self.onvif_port,
                rtsp: self.rtsp_port,
                metrics: self.metrics_port,
            },
            rtsp: OnboardingRtsp {
                path: self.rtsp_path.clone(),
                auth_required,
                username: username.clone(),
                password: password.clone(),
                url: rtsp_url,
            },
            onvif: OnboardingOnvif {
                xaddr: format!("http://{host_ip}:{}/onvif/device_service", self.onvif_port),
                auth_required,
                username,
                password,
            },
            api_token: self.api_token.clone(),
            group_id: self.group_id.clone(),
            node_id: self.node_id.clone(),
            mqtt: self.mqtt.as_ref().map(|m| OnboardingMqttInfo {
                host: m.host.clone(),
                port: m.port,
                tls_enabled: m.tls_enabled,
            }),
        }
    }
}

/// Render `payload` (its JSON encoding) as a PNG QR code. Errors are
/// stringified for the HTTP layer to turn into a 500 — the request body
/// (a short JSON blob) is well within QR capacity, so failure here means a
/// genuine encoder bug, not oversized input.
pub fn render_qr_png(payload: &OnboardingPayload) -> Result<Vec<u8>, String> {
    let json = serde_json::to_string(payload).map_err(|e| e.to_string())?;
    let code = qrcode::QrCode::new(json.as_bytes()).map_err(|e| e.to_string())?;
    let image = code
        .render::<image::Luma<u8>>()
        .quiet_zone(true)
        .module_dimensions(6, 6)
        .build();
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(image)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(png)
}

/// `{"ok": false, "error": ...}` JSON body for auth/error responses — same
/// shape as commands.rs's rejection payloads, so app-side error handling
/// doesn't need a second code path.
pub fn error_json(message: &str) -> String {
    json!({"ok": false, "error": message}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> OnboardingContext {
        OnboardingContext {
            enabled: true,
            device_id: "cam-test".to_string(),
            facility_id: "site-1".to_string(),
            manufacturer: "IIoTEdge".to_string(),
            model: "IIoTEdge Vision Node".to_string(),
            onvif_port: 8000,
            rtsp_port: 8554,
            rtsp_path: "/live".to_string(),
            metrics_port: 9100,
            credentials: Some(("admin".to_string(), "8506".to_string())),
            api_token: "phone-secret".to_string(),
            command_token: "installer-secret".to_string(),
            group_id: "iiotedge".to_string(),
            node_id: "edge-node-1".to_string(),
            mqtt: Some(OnboardingMqtt {
                host: "198.51.100.42".to_string(),
                port: 1883,
                tls_enabled: false,
            }),
        }
    }

    #[test]
    fn empty_command_token_means_open() {
        let mut c = ctx();
        c.command_token.clear();
        assert!(c.authorized(""));
        assert!(c.authorized("anything"));
    }

    #[test]
    fn command_token_gates_access() {
        let c = ctx();
        assert!(c.authorized("installer-secret"));
        assert!(!c.authorized("wrong"));
        assert!(!c.authorized(""));
    }

    #[test]
    fn payload_embeds_credentials_and_api_token() {
        let payload = ctx().payload("192.168.1.7");
        assert_eq!(payload.device_id, "cam-test");
        assert_eq!(payload.host, "192.168.1.7");
        assert_eq!(payload.rtsp.username.as_deref(), Some("admin"));
        assert_eq!(payload.rtsp.password.as_deref(), Some("8506"));
        assert_eq!(payload.rtsp.url, "rtsp://admin:8506@192.168.1.7:8554/live");
        assert_eq!(
            payload.onvif.xaddr,
            "http://192.168.1.7:8000/onvif/device_service"
        );
        assert_eq!(payload.api_token, "phone-secret");
        // The gate secret must never leak into the payload the app receives.
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains("installer-secret"));
    }

    #[test]
    fn payload_embeds_broker_and_node_identity_separately_from_device_id() {
        let payload = ctx().payload("192.0.2.10");
        // The whole point: MQTT broker must NOT default to the camera's own
        // LAN host — real deployments run it elsewhere entirely.
        let mqtt = payload
            .mqtt
            .expect("mqtt present when telemetry configured");
        assert_eq!(mqtt.host, "198.51.100.42");
        assert_ne!(mqtt.host, payload.host);
        assert_eq!(mqtt.port, 1883);
        assert_eq!(payload.group_id, "iiotedge");
        assert_eq!(payload.node_id, "edge-node-1");
    }

    #[test]
    fn payload_omits_mqtt_when_none_configured() {
        let mut c = ctx();
        c.mqtt = None;
        let payload = c.payload("192.0.2.10");
        assert!(payload.mqtt.is_none());
    }

    #[test]
    fn payload_without_users_has_no_credentials() {
        let mut c = ctx();
        c.credentials = None;
        let payload = c.payload("192.168.1.7");
        assert!(!payload.rtsp.auth_required);
        assert!(payload.rtsp.username.is_none());
        assert_eq!(payload.rtsp.url, "rtsp://192.168.1.7:8554/live");
    }

    #[test]
    fn renders_a_valid_png() {
        let payload = ctx().payload("192.168.1.7");
        let png = render_qr_png(&payload).expect("qr render succeeds");
        // PNG magic bytes — enough to confirm this is real image data, not
        // an accidental empty/garbage buffer.
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );
    }
}
