// src/mqtt_bridge.rs
//
// Generic MQTT-JSON southbound bridge (Phase 19b): subscribes to an
// existing MQTT-based device bridge -- Zigbee2MQTT, Z-Wave JS UI, or any
// other JSON-over-MQTT source -- and feeds every message into the SAME
// [[correlation.rules]] engine already wired to industrial southbound
// tags (src/correlation.rs), not a new evidence/action mechanism. A
// Zigbee door sensor's "open" event correlates with video exactly like a
// Modbus PLC reject signal does today: same rule shape, same
// snapshot/clip actions, just a different wire source.
//
// Deliberately not a native Zigbee/Z-Wave radio stack: no mature Rust
// MAC/PHY crate at production quality, no radio hardware on the
// reference device, and Zigbee2MQTT/Z-Wave JS UI are already the de
// facto standard open-source bridges most Home-Assistant-adjacent sites
// already run -- bridging to those is far less risk than reimplementing
// a radio protocol stack (same "don't build infrastructure blind" call
// as TODO.md Phase 12c/12d).
//
// The correlation `source_id` for every event this bridge delivers is
// the raw MQTT topic it arrived on (e.g. "zigbee2mqtt/front_door"), so
// [[correlation.rules]] `source_prefix` matches it exactly the same way
// it already matches "serial/scanner1" or "modbus/plc1/reject_flag".
use crate::config::MqttBridgeSource;
use crate::correlation::CorrelationProcessor;

use bytes::Bytes;
use iiotedge_core::traits::Processor;
use iiotedge_core::types::{ContentType, ProtocolType, UnifiedPayload};
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

pub fn spawn(source: MqttBridgeSource, correlation: Arc<CorrelationProcessor>) {
    let name = source.name.clone();
    let spawned = thread::Builder::new()
        .name(format!("mqtt_bridge_{name}"))
        .spawn(move || run(source, &correlation));
    if let Err(e) = spawned {
        warn!("failed to spawn mqtt_bridge '{name}': {e}");
    }
}

fn run(source: MqttBridgeSource, correlation: &Arc<CorrelationProcessor>) {
    let mut options = MqttOptions::new(
        format!("fusion-firmware-mqtt-bridge-{}", source.name),
        source.host.clone(),
        source.port,
    );
    options.set_keep_alive(Duration::from_secs(30));
    if !source.username.is_empty() {
        options.set_credentials(source.username.clone(), source.password.clone());
    }

    let (client, mut connection) = Client::new(options, 32);
    info!(
        name = %source.name,
        host = %source.host,
        port = source.port,
        topic_filter = %source.topic_filter,
        "MQTT bridge connecting"
    );

    for event in connection.iter() {
        match event {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                if let Err(e) = client.subscribe(&source.topic_filter, QoS::AtMostOnce) {
                    warn!(name = %source.name, "MQTT bridge subscribe failed: {e}");
                } else {
                    info!(name = %source.name, topic_filter = %source.topic_filter, "MQTT bridge subscribed");
                }
            }
            Ok(Event::Incoming(Packet::Publish(publish))) => {
                deliver(correlation, &publish.topic, publish.payload);
            }
            Ok(_) => {}
            Err(e) => {
                warn!(name = %source.name, "MQTT bridge connection error: {e}");
            }
        }
    }
}

/// Wraps one incoming message as a `UnifiedPayload` and hands it straight
/// to the correlation processor's `Processor::process` -- the exact same
/// call the iiotedge-lib engine's own ingest path makes for a genuine
/// southbound driver event, just invoked directly instead of through the
/// engine (this bridge isn't a southbound driver registered in
/// edge.toml/iiotedge-protocols, so there's no engine ingest path to ride).
fn deliver(correlation: &Arc<CorrelationProcessor>, topic: &str, payload: Bytes) {
    let unified = UnifiedPayload::now(topic, ProtocolType::HostApp, ContentType::Json, payload);
    correlation.process(unified);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CorrelationConfig;

    #[test]
    fn zigbee_style_json_event_matches_a_correlation_rule() {
        let (correlation, rx) = CorrelationProcessor::new(&CorrelationConfig {
            enabled: true,
            tolerance_ms: 200,
            rules: vec![crate::config::CorrelationRule {
                name: "front_door_opened".into(),
                source_prefix: "zigbee2mqtt/front_door".into(),
                contains: "\"contact\":false".into(),
                snapshot: true,
                clip: true,
            }],
        })
        .expect("enabled with rules");

        // Zigbee2MQTT's real JSON shape: {"contact":false,"battery":87,...}
        deliver(
            &correlation,
            "zigbee2mqtt/front_door",
            Bytes::from(r#"{"contact":false,"battery":87}"#),
        );

        let hit = rx.try_recv().expect("correlation hit expected");
        assert_eq!(hit.rule, "front_door_opened");
        assert_eq!(hit.source_id, "zigbee2mqtt/front_door");
        assert!(hit.snapshot && hit.clip);
    }

    #[test]
    fn non_matching_topic_produces_no_hit() {
        let (correlation, rx) = CorrelationProcessor::new(&CorrelationConfig {
            enabled: true,
            tolerance_ms: 200,
            rules: vec![crate::config::CorrelationRule {
                name: "front_door_opened".into(),
                source_prefix: "zigbee2mqtt/front_door".into(),
                contains: String::new(),
                snapshot: false,
                clip: false,
            }],
        })
        .expect("enabled with rules");

        deliver(
            &correlation,
            "zigbee2mqtt/back_door",
            Bytes::from(r#"{"contact":false}"#),
        );

        assert!(rx.try_recv().is_err(), "different topic must not match");
    }
}
