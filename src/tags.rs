// src/tags.rs
//
// `[[tags]]`: southbound machine/bridge events -> named signals.
//
// The firmware already receives machine data — Modbus register reads, serial
// lines, CAN frames, Zigbee2MQTT messages — as `UnifiedPayload`s that pass
// through a chain of `Processor`s (the correlation tap, the on-video widgets).
// What it did NOT have was a way to turn that data into something the generic
// side can use: a Matter sensor endpoint reads a *signal* (src/signals.rs), and
// nothing connected a Modbus power meter to one without an external script
// polling and pushing over HTTP.
//
// A `TagProcessor` is that connection. Each `[[tags]]` entry names a southbound
// `source_id`, says how to read a value out of its payload (a typed Modbus
// register, a text number, a JSON field) and which `push:` signal to feed:
//
//   [[tags]]
//   signal = "meter_power_w"
//   source = "modbus/plc1/meter"      # event source id, matched exactly
//   type   = "f32"                    # u16 i16 u32 i32 f32 bool text json
//   offset = 4                        # byte offset (register N = offset 2*N)
//   word_order = "swap"               # 32-bit: low register first (CDAB)
//   scale  = 1.0
//
// and a Matter endpoint then says `source = "push:meter_power_w"`. One firmware
// binary becomes a Modbus-power-meter-to-Matter (or Zigbee-sensor-to-Matter)
// gateway purely by config.
//
// HONESTY. A decode that does not fit — payload shorter than the type needs, a
// field that is missing or not a number, a non-finite float — updates NOTHING:
// the signal keeps its previous value only until it expires. Every tag-fed
// signal has a `max_age_s` (default 60 s): once the link has been silent that
// long the signal reads "no data" (a Matter null), never the last value
// forever. A PLC connection that drops must not look like a sensor that is
// perfectly steady.
//
// The processor is cheap and non-blocking (a hash lookup per event, a decode
// only for matching sources) as the `Processor` contract demands, and passes
// every event through untouched — it observes, never filters.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use iiotedge_core::traits::Processor;
use iiotedge_core::types::UnifiedPayload;
use tracing::info;

use crate::config::TagConfig;
use crate::signals::{PushedSource, SignalBus, Value};

/// How a tag reads its value out of the payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataType {
    U16,
    I16,
    U32,
    I32,
    F32,
    Bool,
    Text,
    Json,
}

impl DataType {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "u16" => Self::U16,
            "i16" => Self::I16,
            "u32" => Self::U32,
            "i32" => Self::I32,
            "f32" => Self::F32,
            "bool" => Self::Bool,
            "text" => Self::Text,
            "json" => Self::Json,
            _ => return None,
        })
    }

    pub(crate) fn names() -> &'static str {
        "u16, i16, u32, i32, f32, bool, text, json"
    }

    /// Spans two Modbus registers, so word order matters.
    pub(crate) fn is_32_bit(self) -> bool {
        matches!(self, Self::U32 | Self::I32 | Self::F32)
    }
}

/// Register order of a 32-bit value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WordOrder {
    /// High register first ("ABCD"): the Modbus convention.
    Big,
    /// Low register first ("CDAB"): common on meters and some PLCs.
    Swap,
}

impl WordOrder {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "" | "big" => Some(Self::Big),
            "swap" => Some(Self::Swap),
            _ => None,
        }
    }
}

struct Tag {
    cell: Arc<PushedSource>,
    data_type: DataType,
    offset: usize,
    word_order: WordOrder,
    field: String,
    scale: f64,
    bias: f64,
}

/// Feeds `push:` signals from southbound events. Register it as a `Processor`
/// in the telemetry ingest path (and hand it to the MQTT bridge, which delivers
/// straight to its processors).
pub struct TagProcessor {
    by_source: HashMap<String, Vec<Tag>>,
}

impl TagProcessor {
    /// `None` when there are no tags (nothing to register). Declares every tag's
    /// signal on the bus with its expiry; call at startup, before Matter binds
    /// endpoints to the names.
    pub fn new(tags: &[TagConfig], bus: &SignalBus) -> Option<Arc<Self>> {
        if tags.is_empty() {
            return None;
        }
        let mut by_source: HashMap<String, Vec<Tag>> = HashMap::new();
        for t in tags {
            // Validated at config load; fall back to a safe default rather than
            // panic if a caller skipped validation.
            let data_type = DataType::parse(&t.data_type).unwrap_or(DataType::Text);
            by_source.entry(t.source.clone()).or_default().push(Tag {
                cell: bus.declare_pushed(&t.signal, Duration::from_secs(t.max_age_s)),
                data_type,
                offset: t.offset,
                word_order: WordOrder::parse(&t.word_order).unwrap_or(WordOrder::Big),
                field: t.field.clone(),
                scale: t.scale,
                bias: t.bias,
            });
            info!(signal = %t.signal, source = %t.source, r#type = %t.data_type, "Tag: signal bound to a southbound source");
        }
        Some(Arc::new(Self { by_source }))
    }
}

impl Processor for TagProcessor {
    fn process(&self, payload: UnifiedPayload) -> Option<UnifiedPayload> {
        if let Some(tags) = self.by_source.get(&payload.source_id) {
            for tag in tags {
                if let Some(value) = decode(tag, &payload.payload) {
                    tag.cell.set(value);
                }
            }
        }
        // Observe, never filter.
        Some(payload)
    }
}

/// `payload[offset..offset+N]`, or `None` if the payload is too short.
fn take<const N: usize>(payload: &[u8], offset: usize) -> Option<[u8; N]> {
    payload.get(offset..offset.checked_add(N)?)?.try_into().ok()
}

/// The two registers at `offset` as one 32-bit value, honouring word order.
fn u32_at(payload: &[u8], offset: usize, order: WordOrder) -> Option<u32> {
    let [a, b, c, d] = take::<4>(payload, offset)?;
    let (hi, lo) = (u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d]));
    Some(match order {
        WordOrder::Big => (u32::from(hi) << 16) | u32::from(lo),
        WordOrder::Swap => (u32::from(lo) << 16) | u32::from(hi),
    })
}

/// A number (or numeric string) / bool at the dotted `path` of a JSON document.
fn json_value(payload: &[u8], path: &str) -> Option<Value> {
    let mut node: &serde_json::Value = &serde_json::from_slice(payload).ok()?;
    for segment in path.split('.') {
        node = match segment.parse::<usize>() {
            Ok(index) if node.is_array() => node.get(index)?,
            _ => node.get(segment)?,
        };
    }
    match node {
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Number(n) => n.as_f64().map(Value::Num),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok().map(Value::Num),
        _ => None,
    }
}

/// A trimmed number, or one of true/false/on/off, from a text payload.
fn text_value(payload: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(payload).ok()?.trim();
    match text.to_ascii_lowercase().as_str() {
        "true" | "on" => Some(Value::Bool(true)),
        "false" | "off" => Some(Value::Bool(false)),
        _ => text.parse::<f64>().ok().map(Value::Num),
    }
}

/// Read one tag's value out of `payload`. `None` when it does not fit (too
/// short, missing field, non-finite) — the caller then updates nothing.
fn decode(tag: &Tag, payload: &[u8]) -> Option<Value> {
    let raw = match tag.data_type {
        DataType::U16 => f64::from(u16::from_be_bytes(take::<2>(payload, tag.offset)?)),
        DataType::I16 => f64::from(i16::from_be_bytes(take::<2>(payload, tag.offset)?)),
        DataType::U32 => f64::from(u32_at(payload, tag.offset, tag.word_order)?),
        DataType::I32 => f64::from(u32_at(payload, tag.offset, tag.word_order)? as i32),
        DataType::F32 => f64::from(f32::from_bits(u32_at(payload, tag.offset, tag.word_order)?)),
        DataType::Bool => {
            return Some(Value::Bool(*payload.get(tag.offset)? != 0));
        }
        DataType::Text => match text_value(payload)? {
            Value::Bool(b) => return Some(Value::Bool(b)),
            Value::Num(n) => n,
        },
        DataType::Json => match json_value(payload, &tag.field)? {
            Value::Bool(b) => return Some(Value::Bool(b)),
            Value::Num(n) => n,
        },
    };
    let scaled = raw * tag.scale + tag.bias;
    scaled.is_finite().then_some(Value::Num(scaled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use iiotedge_core::types::{ContentType, ProtocolType};

    fn tag(data_type: DataType) -> Tag {
        Tag {
            cell: Arc::new(PushedSource::new("t", None)),
            data_type,
            offset: 0,
            word_order: WordOrder::Big,
            field: String::new(),
            scale: 1.0,
            bias: 0.0,
        }
    }

    fn num(v: Option<Value>) -> Option<f64> {
        v.map(|v| v.as_f64())
    }

    #[test]
    fn sixteen_bit_registers_are_big_endian_signed_or_not() {
        assert_eq!(num(decode(&tag(DataType::U16), &[0x01, 0x2C])), Some(300.0));
        assert_eq!(num(decode(&tag(DataType::U16), &[0xFF, 0xFF])), Some(65535.0));
        assert_eq!(num(decode(&tag(DataType::I16), &[0xFF, 0x9C])), Some(-100.0));
        assert_eq!(num(decode(&tag(DataType::I16), &[0x7F, 0xFF])), Some(32767.0));
    }

    #[test]
    fn thirty_two_bit_values_honour_word_order() {
        // 100000 = 0x0001_86A0 -> registers [0x0001, 0x86A0]
        let big = [0x00, 0x01, 0x86, 0xA0];
        let swapped = [0x86, 0xA0, 0x00, 0x01];
        let mut t = tag(DataType::U32);
        assert_eq!(num(decode(&t, &big)), Some(100_000.0), "ABCD");
        assert_eq!(
            num(decode(&t, &swapped)),
            Some(f64::from(0x86A0_0001u32)),
            "CDAB bytes read as ABCD are a different number: the order setting matters"
        );
        t.word_order = WordOrder::Swap;
        assert_eq!(num(decode(&t, &swapped)), Some(100_000.0), "CDAB");
    }

    #[test]
    fn signed_32_bit_and_floats() {
        // -2 = 0xFFFF_FFFE
        assert_eq!(num(decode(&tag(DataType::I32), &[0xFF, 0xFF, 0xFF, 0xFE])), Some(-2.0));
        // 21.5f32 = 0x41AC_0000
        let bits = 21.5f32.to_bits().to_be_bytes();
        assert_eq!(num(decode(&tag(DataType::F32), &bits)), Some(21.5));
        let mut t = tag(DataType::F32);
        t.word_order = WordOrder::Swap;
        let swapped = [bits[2], bits[3], bits[0], bits[1]];
        assert_eq!(num(decode(&t, &swapped)), Some(21.5));
    }

    #[test]
    fn the_offset_picks_a_register_out_of_a_longer_read() {
        // A 6-register read: voltage (f32 @0), current (f32 @4), a counter (u16 @8).
        let mut payload = Vec::new();
        payload.extend_from_slice(&230.5f32.to_bits().to_be_bytes());
        payload.extend_from_slice(&2.25f32.to_bits().to_be_bytes());
        payload.extend_from_slice(&[0x00, 0x2A]);
        let mut volts = tag(DataType::F32);
        let mut amps = tag(DataType::F32);
        amps.offset = 4;
        let mut count = tag(DataType::U16);
        count.offset = 8;
        volts.offset = 0;
        assert_eq!(num(decode(&volts, &payload)), Some(230.5));
        assert_eq!(num(decode(&amps, &payload)), Some(2.25));
        assert_eq!(num(decode(&count, &payload)), Some(42.0));
    }

    #[test]
    fn a_payload_too_short_for_the_type_decodes_to_nothing() {
        assert!(decode(&tag(DataType::U16), &[0x01]).is_none());
        assert!(decode(&tag(DataType::U32), &[0x00, 0x01, 0x86]).is_none());
        let mut t = tag(DataType::U16);
        t.offset = 4;
        assert!(decode(&t, &[0x00, 0x01, 0x02, 0x03]).is_none(), "offset past the end");
        t.offset = usize::MAX;
        assert!(decode(&t, &[0x00, 0x01]).is_none(), "offset arithmetic must not overflow");
        assert!(decode(&tag(DataType::Bool), &[]).is_none());
    }

    #[test]
    fn scale_and_bias_apply_to_numbers_only() {
        let mut t = tag(DataType::I16);
        t.scale = 0.1;
        t.bias = -5.0;
        // raw 300 -> 300*0.1 - 5 = 25
        assert_eq!(num(decode(&t, &[0x01, 0x2C])), Some(25.0));
        let mut b = tag(DataType::Bool);
        b.scale = 10.0;
        assert_eq!(decode(&b, &[1]), Some(Value::Bool(true)), "a bool is not scaled");
    }

    #[test]
    fn a_non_finite_result_is_rejected() {
        // f32 NaN and +inf
        assert!(decode(&tag(DataType::F32), &f32::NAN.to_bits().to_be_bytes()).is_none());
        assert!(decode(&tag(DataType::F32), &f32::INFINITY.to_bits().to_be_bytes()).is_none());
        let mut t = tag(DataType::U16);
        t.scale = f64::MAX;
        assert!(decode(&t, &[0xFF, 0xFF]).is_none(), "overflow to infinity");
    }

    #[test]
    fn coils_are_one_byte_each() {
        let mut t = tag(DataType::Bool);
        assert_eq!(decode(&t, &[0, 1, 0]), Some(Value::Bool(false)));
        t.offset = 1;
        assert_eq!(decode(&t, &[0, 1, 0]), Some(Value::Bool(true)));
    }

    #[test]
    fn serial_style_text() {
        assert_eq!(num(decode(&tag(DataType::Text), b"21.5\r\n")), Some(21.5));
        assert_eq!(num(decode(&tag(DataType::Text), b"  -7 ")), Some(-7.0));
        assert_eq!(decode(&tag(DataType::Text), b"ON"), Some(Value::Bool(true)));
        assert_eq!(decode(&tag(DataType::Text), b"off\n"), Some(Value::Bool(false)));
        assert!(decode(&tag(DataType::Text), b"E-ROR").is_none(), "not a number");
        assert!(decode(&tag(DataType::Text), &[0xFF, 0xFE]).is_none(), "not UTF-8");
    }

    #[test]
    fn zigbee2mqtt_style_json() {
        let json = br#"{"temperature": 21.5, "humidity": "40", "contact": false,
                        "linkquality": 87, "data": {"power": 12.5, "list": [3, 4]}}"#;
        let with = |field: &str| {
            let mut t = tag(DataType::Json);
            t.field = field.to_string();
            decode(&t, json)
        };
        assert_eq!(num(with("temperature")), Some(21.5));
        assert_eq!(num(with("humidity")), Some(40.0), "a numeric string is a number");
        assert_eq!(with("contact"), Some(Value::Bool(false)));
        assert_eq!(num(with("data.power")), Some(12.5), "dotted path");
        assert_eq!(num(with("data.list.1")), Some(4.0), "array index");
        assert!(with("missing").is_none());
        assert!(with("data").is_none(), "an object is not a value");
        let mut t = tag(DataType::Json);
        t.field = "temperature".into();
        assert!(decode(&t, b"not json").is_none());
        assert!(decode(&t, br#"{"temperature": null}"#).is_none());
    }

    fn cfg(signal: &str, source: &str, data_type: &str) -> TagConfig {
        TagConfig {
            signal: signal.into(),
            source: source.into(),
            data_type: data_type.into(),
            offset: 0,
            word_order: String::new(),
            field: String::new(),
            scale: 1.0,
            bias: 0.0,
            max_age_s: 60,
        }
    }

    fn event(source: &str, bytes: &[u8]) -> UnifiedPayload {
        UnifiedPayload::now(
            source,
            ProtocolType::Modbus,
            ContentType::OctetStream,
            Bytes::copy_from_slice(bytes),
        )
    }

    #[test]
    fn no_tags_means_no_processor() {
        assert!(TagProcessor::new(&[], &SignalBus::new()).is_none());
    }

    #[test]
    fn events_feed_the_named_signals_and_pass_through_untouched() {
        let bus = SignalBus::new();
        let mut volts = cfg("grid_volts", "modbus/meter/block", "f32");
        volts.word_order = "swap".into();
        let mut amps = cfg("grid_amps", "modbus/meter/block", "u16");
        amps.offset = 4;
        amps.scale = 0.01;
        let proc_ = TagProcessor::new(&[volts, amps, cfg("temp", "serial/probe", "text")], &bus).unwrap();

        let read = |name: &str| bus.resolve(&crate::signals::parse_spec(&format!("push:{name}")).unwrap()).unwrap().read();
        assert!(read("grid_volts").is_none(), "no reading before the first event");

        // 230.5 V as two swapped registers, then 1234 (= 12.34 A at scale 0.01).
        let bits = 230.5f32.to_bits().to_be_bytes();
        let mut payload = vec![bits[2], bits[3], bits[0], bits[1]];
        payload.extend_from_slice(&1234u16.to_be_bytes());
        let original = event("modbus/meter/block", &payload);
        let out = proc_.process(original.clone());
        assert_eq!(out.as_ref(), Some(&original), "observe, never filter");
        assert_eq!(read("grid_volts").unwrap().value, Value::Num(230.5));
        assert!((read("grid_amps").unwrap().value.as_f64() - 12.34).abs() < 1e-9);

        // An unrelated source touches nothing; a matching one with a payload that
        // doesn't fit updates nothing (the signal keeps its value until it expires).
        proc_.process(event("modbus/other/block", &[0xFF; 8]));
        proc_.process(event("modbus/meter/block", &[0x01]));
        assert_eq!(read("grid_volts").unwrap().value, Value::Num(230.5));

        proc_.process(event("serial/probe", b"19.25\n"));
        assert_eq!(read("temp").unwrap().value, Value::Num(19.25));
    }

    #[test]
    fn a_silent_link_expires_to_no_data() {
        // The expiry itself is a PushedSource property (tested in signals); this
        // checks the processor declares every tag's signal WITH one.
        let bus = SignalBus::new();
        let mut t = cfg("sig", "modbus/a/b", "u16");
        t.max_age_s = 1;
        let proc_ = TagProcessor::new(&[t], &bus).unwrap();
        proc_.process(event("modbus/a/b", &[0x00, 0x07]));
        let src = bus.resolve(&crate::signals::parse_spec("push:sig").unwrap()).unwrap();
        assert_eq!(src.read().unwrap().value, Value::Num(7.0));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(src.read().is_none(), "no update for max_age_s -> no data, not the last value");
    }
}
