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

use bytes::Bytes;
use iiotedge_core::traits::Processor;
use iiotedge_core::types::{ContentType, ProtocolType, UnifiedPayload};
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

/// `sinks` are the consumers of machine data (the correlation tap, the
/// `[[tags]]` processor): every message is delivered to each of them.
pub fn spawn(source: MqttBridgeSource, sinks: Vec<Arc<dyn Processor>>) {
    let name = source.name.clone();
    let spawned = thread::Builder::new()
        .name(format!("mqtt_bridge_{name}"))
        .spawn(move || run(source, &sinks));
    if let Err(e) = spawned {
        warn!("failed to spawn mqtt_bridge '{name}': {e}");
    }
}

fn run(source: MqttBridgeSource, sinks: &[Arc<dyn Processor>]) {
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
                deliver(sinks, &publish.topic, publish.payload);
            }
            Ok(_) => {}
            Err(e) => {
                warn!(name = %source.name, "MQTT bridge connection error: {e}");
            }
        }
    }
}

/// Wraps one incoming message as a `UnifiedPayload` and hands it straight
/// to each sink's `Processor::process` -- the exact same call the
/// iiotedge-lib engine's own ingest path makes for a genuine southbound
/// driver event, just invoked directly instead of through the engine (this
/// bridge isn't a southbound driver registered in
/// edge.toml/iiotedge-protocols, so there's no engine ingest path to ride).
fn deliver(sinks: &[Arc<dyn Processor>], topic: &str, payload: Bytes) {
    let unified = UnifiedPayload::now(topic, ProtocolType::HostApp, ContentType::Json, payload);
    for sink in sinks {
        sink.process(unified.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CorrelationConfig;
    use crate::correlation::CorrelationProcessor;

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
            &[correlation as Arc<dyn Processor>],
            "zigbee2mqtt/front_door",
            Bytes::from(r#"{"contact":false,"battery":87}"#),
        );

        let hit = rx.try_recv().expect("correlation hit expected");
        assert_eq!(hit.rule, "front_door_opened");
        assert_eq!(hit.source_id, "zigbee2mqtt/front_door");
        assert!(hit.snapshot && hit.clip);
    }

    #[test]
    fn a_zigbee_message_feeds_a_tag_signal_as_well_as_correlation() {
        use crate::config::TagConfig;
        use crate::signals::{parse_spec, SignalBus, Value};
        let bus = SignalBus::new();
        let tags = crate::tags::TagProcessor::new(
            &[TagConfig {
                signal: "garage_temp".into(),
                source: "zigbee2mqtt/garage".into(),
                data_type: "json".into(),
                offset: 0,
                word_order: String::new(),
                field: "temperature".into(),
                scale: 1.0,
                bias: 0.0,
                max_age_s: 3600,
            }],
            &bus,
        )
        .expect("one tag");
        // Zigbee2MQTT's real JSON shape.
        deliver(
            &[tags as Arc<dyn Processor>],
            "zigbee2mqtt/garage",
            Bytes::from(r#"{"temperature":4.5,"humidity":71,"battery":93,"linkquality":120}"#),
        );
        let src = bus.resolve(&parse_spec("push:garage_temp").unwrap()).unwrap();
        assert_eq!(src.read().unwrap().value, Value::Num(4.5));
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
            &[correlation as Arc<dyn Processor>],
            "zigbee2mqtt/back_door",
            Bytes::from(r#"{"contact":false}"#),
        );

        assert!(rx.try_recv().is_err(), "different topic must not match");
    }
}
