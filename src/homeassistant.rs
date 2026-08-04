// src/homeassistant.rs
//
// Home Assistant MQTT Discovery (Phase 19a): publishes retained HA
// discovery config messages, then live entity state, on the SAME
// broker/identity as the command channel (src/commands.rs) -- not a
// second broker -- because HA's button entities need their
// command_topic to land on the exact topic commands.rs already
// subscribes to for "press a button in HA, it runs a real command" to
// work with zero new command-ingestion code.
//
// Discovery topic format per the HA spec:
// <discovery_prefix>/<component>/<node_id>/<object_id>/config
// https://www.home-assistant.io/integrations/mqtt/
use crate::config::HomeAssistantConfig;

use iiotedge_core::EdgeConfig;
use rumqttc::{Client, MqttOptions, QoS};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

/// A live entity-state update to publish. Discovery itself (the
/// `.../config` retained messages) is published once at connect from the
/// rule/zone names known at construction time -- this only carries what
/// changes afterward.
enum HaUpdate {
    Tamper(bool),
    Motion { zone: String, active: bool },
    Rule(String),
}

pub struct HomeAssistantBridge {
    tx: mpsc::SyncSender<HaUpdate>,
}

impl HomeAssistantBridge {
    /// `rule_names`/`motion_zones` are the object_ids discovery announces
    /// up front -- known at boot from config, not learned at runtime.
    pub fn spawn(
        cfg: &HomeAssistantConfig,
        edge_config_path: &str,
        device_id: &str,
        command_token: &str,
        rule_names: Vec<String>,
        motion_zones: Vec<String>,
    ) -> Option<Arc<Self>> {
        if !cfg.enabled {
            return None;
        }
        let edge = match EdgeConfig::from_file(edge_config_path) {
            Ok(e) => e,
            Err(e) => {
                warn!("Home Assistant bridge disabled: cannot load {edge_config_path}: {e}");
                return None;
            }
        };
        let cfg = cfg.clone();
        let device_id = device_id.to_string();
        let command_token = command_token.to_string();
        let (tx, rx) = mpsc::sync_channel::<HaUpdate>(32);
        let spawned = thread::Builder::new()
            .name("ha_discovery".to_string())
            .spawn(move || {
                run(
                    cfg,
                    edge,
                    device_id,
                    command_token,
                    rule_names,
                    motion_zones,
                    rx,
                )
            });
        if let Err(e) = spawned {
            warn!("failed to spawn ha_discovery thread: {e}");
            return None;
        }
        Some(Arc::new(Self { tx }))
    }

    /// Non-blocking; drops (silently -- tamper/motion re-publish on every
    /// analysis tick, so a dropped update self-heals on the next one)
    /// when the queue of 32 pending updates is already full.
    pub fn tamper(&self, active: bool) {
        let _ = self.tx.try_send(HaUpdate::Tamper(active));
    }

    pub fn motion(&self, zone: &str, active: bool) {
        let _ = self.tx.try_send(HaUpdate::Motion {
            zone: zone.to_string(),
            active,
        });
    }

    pub fn rule(&self, name: &str) {
        let _ = self.tx.try_send(HaUpdate::Rule(name.to_string()));
    }
}

/// `<discovery_prefix>/<component>/<device_id>/<object_id>/config`, the
/// exact topic shape HA's MQTT Discovery spec requires.
fn discovery_topic(prefix: &str, component: &str, device_id: &str, object_id: &str) -> String {
    format!("{prefix}/{component}/{device_id}/{object_id}/config")
}

fn state_topic(kind: &str, device_id: &str, object_id: &str) -> String {
    format!("iiotedge/{device_id}/ha/{kind}/{object_id}/state")
}

fn device_block(device_id: &str, device_name: &str) -> Value {
    json!({
        "identifiers": [device_id],
        "name": if device_name.is_empty() { device_id } else { device_name },
        "manufacturer": "IIoTEdge",
        "model": "Fusion Firmware",
    })
}

/// Tamper: one aggregate binary_sensor. The detector distinguishes 5
/// tamper kinds (blackout/blinding/occlusion/freeze/scene_change,
/// src/tamper.rs) but a homeowner cares "is the camera tampered with,"
/// not which mechanism -- collapsing to one entity matches that.
fn tamper_entity(state_topic: &str) -> Value {
    json!({
        "name": "Tamper",
        "device_class": "tamper",
        "state_topic": state_topic,
        "payload_on": "ON",
        "payload_off": "OFF",
    })
}

fn motion_entity(zone: &str, state_topic: &str) -> Value {
    json!({
        "name": format!("Motion \u{2014} {zone}"),
        "device_class": "motion",
        "state_topic": state_topic,
        "payload_on": "ON",
        "payload_off": "OFF",
    })
}

/// AI rules (Phase 16): a rule match is a point-in-time detection, not
/// sustained state, so `off_delay` (HA auto-resets to "off" this many
/// seconds after the last "on") is the correct mapping, not a manual
/// on/off pair the way tamper/motion get.
fn rule_entity(name: &str, state_topic: &str, off_delay_s: u32) -> Value {
    json!({
        "name": format!("Rule \u{2014} {name}"),
        "state_topic": state_topic,
        "payload_on": "ON",
        "payload_off": "OFF",
        "off_delay": off_delay_s,
    })
}

/// Snapshot/clip buttons: command_topic IS the existing MQTT command
/// channel (src/commands.rs's cmd_topic) -- pressing the button in HA
/// publishes the exact JSON handle_command already parses, so no new
/// command-ingestion code is needed for this to actually work.
fn button_entity(name: &str, cmd_topic: &str, cmd: &str, command_token: &str) -> Value {
    json!({
        "name": name,
        "command_topic": cmd_topic,
        "payload_press": json!({"cmd": cmd, "token": command_token}).to_string(),
    })
}

fn on_off(active: bool) -> String {
    (if active { "ON" } else { "OFF" }).to_string()
}

fn run(
    cfg: HomeAssistantConfig,
    edge: EdgeConfig,
    device_id: String,
    command_token: String,
    rule_names: Vec<String>,
    motion_zones: Vec<String>,
    rx: mpsc::Receiver<HaUpdate>,
) {
    let mqtt = &edge.northbound.mqtt;
    let mut options = MqttOptions::new(
        format!("{}-ha", edge.node.node_id),
        mqtt.host.clone(),
        mqtt.port,
    );
    options.set_keep_alive(Duration::from_secs(30));
    crate::core::mqtt_tls::apply(&mut options, &edge.northbound.tls, "home assistant bridge");

    let (client, mut connection) = Client::new(options, 32);

    // rumqttc's Client only queues; Connection must be polled continuously
    // on some thread for anything to actually reach the wire. This thread
    // does nothing but that -- all publishing happens below via `client`.
    thread::Builder::new()
        .name("ha_discovery_io".to_string())
        .spawn(move || {
            for event in connection.iter() {
                if let Err(e) = event {
                    warn!("home assistant MQTT connection error: {e}");
                }
            }
        })
        .ok();

    let device = device_block(&device_id, &cfg.device_name);
    let cmd_topic = format!("iiotedge/{}/{}/cmd", edge.node.group_id, edge.node.node_id);

    let publish_config = |component: &str, object_id: &str, mut entity: Value| {
        if let Value::Object(map) = &mut entity {
            map.insert("device".to_string(), device.clone());
            map.insert(
                "unique_id".to_string(),
                json!(format!("{device_id}_{object_id}")),
            );
        }
        let topic = discovery_topic(&cfg.discovery_prefix, component, &device_id, object_id);
        if let Err(e) = client.publish(&topic, QoS::AtLeastOnce, true, entity.to_string()) {
            warn!("home assistant discovery publish failed for {object_id}: {e}");
        }
    };

    let tamper_topic = state_topic("tamper", &device_id, "tamper");
    publish_config("binary_sensor", "tamper", tamper_entity(&tamper_topic));

    // Motion: one binary_sensor per zone name the caller supplied (already
    // resolved against motion.rs's own empty-config-means-one-implicit-
    // "frame"-zone fallback, and empty here specifically when motion
    // detection itself is off — see the call site in main.rs).
    for zone in &motion_zones {
        let topic = state_topic("motion", &device_id, zone);
        publish_config(
            "binary_sensor",
            &format!("motion_{zone}"),
            motion_entity(zone, &topic),
        );
    }

    for rule in &rule_names {
        let topic = state_topic("rule", &device_id, rule);
        publish_config(
            "binary_sensor",
            &format!("rule_{rule}"),
            rule_entity(rule, &topic, cfg.off_delay_s),
        );
    }

    for (object_id, name, cmd) in [
        ("snapshot", "Snapshot", "snapshot"),
        ("clip", "Clip", "clip"),
    ] {
        publish_config(
            "button",
            object_id,
            button_entity(name, &cmd_topic, cmd, &command_token),
        );
    }

    info!(
        rules = rule_names.len(),
        zones = motion_zones.len(),
        "Home Assistant MQTT Discovery published"
    );

    for update in rx {
        let (topic, payload) = match update {
            HaUpdate::Tamper(active) => (tamper_topic.clone(), on_off(active)),
            HaUpdate::Motion { zone, active } => {
                (state_topic("motion", &device_id, &zone), on_off(active))
            }
            HaUpdate::Rule(name) => (state_topic("rule", &device_id, &name), "ON".to_string()),
        };
        if let Err(e) = client.publish(&topic, QoS::AtMostOnce, false, payload) {
            warn!("home assistant state publish failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_topic_matches_the_ha_spec_shape() {
        assert_eq!(
            discovery_topic("homeassistant", "binary_sensor", "cam-1", "tamper"),
            "homeassistant/binary_sensor/cam-1/tamper/config"
        );
    }

    #[test]
    fn state_topic_is_stable_and_namespaced_per_kind() {
        assert_eq!(
            state_topic("motion", "cam-1", "front_yard"),
            "iiotedge/cam-1/ha/motion/front_yard/state"
        );
        assert_ne!(
            state_topic("rule", "cam-1", "front_yard"),
            state_topic("motion", "cam-1", "front_yard")
        );
    }

    #[test]
    fn rule_entity_carries_off_delay_for_momentary_semantics() {
        let entity = rule_entity(
            "loading_dock",
            "iiotedge/cam-1/ha/rule/loading_dock/state",
            15,
        );
        assert_eq!(entity["off_delay"], 15);
        assert_eq!(entity["payload_on"], "ON");
        assert_eq!(
            entity["state_topic"],
            "iiotedge/cam-1/ha/rule/loading_dock/state"
        );
    }

    #[test]
    fn tamper_entity_has_no_off_delay_since_it_is_sustained_state() {
        let entity = tamper_entity("iiotedge/cam-1/ha/tamper/tamper/state");
        assert!(entity.get("off_delay").is_none());
        assert_eq!(entity["device_class"], "tamper");
    }

    #[test]
    fn button_payload_press_matches_the_real_command_schema() {
        let entity = button_entity("Snapshot", "iiotedge/g1/n1/cmd", "snapshot", "secret-token");
        let payload_press = entity["payload_press"]
            .as_str()
            .expect("string payload_press");
        let parsed: Value = serde_json::from_str(payload_press).expect("valid JSON");
        assert_eq!(parsed["cmd"], "snapshot");
        assert_eq!(parsed["token"], "secret-token");
        assert_eq!(entity["command_topic"], "iiotedge/g1/n1/cmd");
    }

    #[test]
    fn device_block_falls_back_to_device_id_when_name_is_empty() {
        assert_eq!(device_block("cam-1", "")["name"], "cam-1");
        assert_eq!(device_block("cam-1", "Front Porch")["name"], "Front Porch");
    }

    #[test]
    fn on_off_maps_bool_to_ha_payload_strings() {
        assert_eq!(on_off(true), "ON");
        assert_eq!(on_off(false), "OFF");
    }
}
