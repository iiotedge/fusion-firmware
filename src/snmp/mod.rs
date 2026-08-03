// src/snmp/mod.rs
//
// SNMPv2c agent (TODO.md Phase 12, F10) — off by default, most fleets
// monitor over MQTT/GDE telemetry instead; this exists for sites whose
// existing NMS (Zabbix, PRTG, LibreNMS, ...) only speaks SNMP. MIB-II
// system group + a small private enterprise MIB, both backed by the exact
// same counters `/metrics` already tracks (core/metrics.rs) and the
// footprint (footprint.rs) — one source of truth, not a second parallel
// metrics pipeline that could drift from the real one.
//
// v2c only: v3's USM auth/privacy layer is real, separate complexity this
// phase doesn't take on. The community string is the only access control,
// sent in the clear on the wire like the rest of v2c — treat it like every
// other plaintext-LAN-service credential this firmware already has (RTSP/
// ONVIF Basic auth, MQTT without TLS when [northbound.tls] is off).
mod ber;

use crate::config::SnmpConfig;
use crate::core::metrics::Metrics;
use crate::footprint::Footprint;

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

/// IANA Private Enterprise Numbers is a real registry
/// (https://www.iana.org/assignments/enterprise-numbers/) — this repo has
/// no assigned number. 99999 is an obvious placeholder, not a real one;
/// replace it before pointing production NMS tooling at this agent.
const ENTERPRISE_OID: [u32; 7] = [1, 3, 6, 1, 4, 1, 99999];

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Integer(i64),
    OctetString(Vec<u8>),
    Oid(Vec<u32>),
    Counter32(u32),
    Gauge32(u32),
    TimeTicks(u32),
    NoSuchObject,
    EndOfMibView,
}

impl Value {
    fn encode(&self) -> Vec<u8> {
        match self {
            Value::Integer(v) => ber::encode_integer(*v),
            Value::OctetString(v) => ber::encode_octet_string(v),
            Value::Oid(v) => ber::encode_oid(v),
            Value::Counter32(v) => ber::encode_unsigned32(ber::TAG_COUNTER32, *v),
            Value::Gauge32(v) => ber::encode_unsigned32(ber::TAG_GAUGE32, *v),
            Value::TimeTicks(v) => ber::encode_unsigned32(ber::TAG_TIME_TICKS, *v),
            Value::NoSuchObject => ber::tlv(ber::TAG_NO_SUCH_OBJECT, &[]),
            Value::EndOfMibView => ber::tlv(ber::TAG_END_OF_MIB_VIEW, &[]),
        }
    }
}

/// Everything an OID's value function might need to read. Built once at
/// boot and shared read-only — the agent never mutates firmware state,
/// only reports it (no Set support; SetRequest is rejected, see `dispatch`).
#[derive(Clone)]
pub struct SnmpContext {
    pub metrics: Arc<Metrics>,
    pub footprint: Arc<Footprint>,
    pub booted: Instant,
    pub sys_contact: String,
    pub sys_location: String,
    pub telemetry_enabled: bool,
}

type Extractor = fn(&SnmpContext) -> Value;

/// Sorted ascending by OID — required for GetNextRequest's "first entry
/// strictly greater than the requested OID" walk to produce a correct,
/// monotonically-increasing sequence (what `snmpwalk` relies on to know
/// when to stop).
fn oid_table() -> [(&'static [u32], Extractor); 13] {
    [
        (&[1, 3, 6, 1, 2, 1, 1, 1, 0], sys_descr),
        (&[1, 3, 6, 1, 2, 1, 1, 2, 0], sys_object_id),
        (&[1, 3, 6, 1, 2, 1, 1, 3, 0], sys_up_time),
        (&[1, 3, 6, 1, 2, 1, 1, 4, 0], sys_contact),
        (&[1, 3, 6, 1, 2, 1, 1, 5, 0], sys_name),
        (&[1, 3, 6, 1, 2, 1, 1, 6, 0], sys_location),
        (&[1, 3, 6, 1, 2, 1, 1, 7, 0], sys_services),
        (&[1, 3, 6, 1, 4, 1, 99999, 1, 1, 0], frames_captured),
        (&[1, 3, 6, 1, 4, 1, 99999, 1, 2, 0], frames_dropped),
        (&[1, 3, 6, 1, 4, 1, 99999, 1, 3, 0], tamper_active),
        (&[1, 3, 6, 1, 4, 1, 99999, 1, 4, 0], storage_free_mb),
        (&[1, 3, 6, 1, 4, 1, 99999, 1, 5, 0], storage_used_mb),
        (&[1, 3, 6, 1, 4, 1, 99999, 1, 6, 0], telemetry_enabled_value),
    ]
}

fn sys_descr(ctx: &SnmpContext) -> Value {
    Value::OctetString(
        format!(
            "Fusion Firmware {} ({}) - {}",
            ctx.footprint.firmware_version, ctx.footprint.git_hash, ctx.footprint.model
        )
        .into_bytes(),
    )
}
fn sys_object_id(_ctx: &SnmpContext) -> Value {
    Value::Oid(ENTERPRISE_OID.to_vec())
}
fn sys_up_time(ctx: &SnmpContext) -> Value {
    let centiseconds = ctx.booted.elapsed().as_millis() / 10;
    // Counter32/TimeTicks wrap at 2^32 by design (a real, accepted MIB-II
    // limitation, ~497 days here) -- not a bug to work around.
    Value::TimeTicks(u32::try_from(centiseconds).unwrap_or(u32::MAX))
}
fn sys_contact(ctx: &SnmpContext) -> Value {
    Value::OctetString(ctx.sys_contact.clone().into_bytes())
}
fn sys_name(ctx: &SnmpContext) -> Value {
    Value::OctetString(ctx.footprint.device_id.clone().into_bytes())
}
fn sys_location(ctx: &SnmpContext) -> Value {
    Value::OctetString(ctx.sys_location.clone().into_bytes())
}
fn sys_services(_ctx: &SnmpContext) -> Value {
    // RFC 1213 convention for an application-layer-only device: 2^(7-1).
    Value::Integer(72)
}
fn frames_captured(ctx: &SnmpContext) -> Value {
    // IntCounter::get() is u64 (monotonic, never negative) -- unlike the
    // IntGauge fields below, no .max(0) clamp needed or meaningful here.
    Value::Counter32(u32::try_from(ctx.metrics.frames_captured.get()).unwrap_or(u32::MAX))
}
fn frames_dropped(ctx: &SnmpContext) -> Value {
    Value::Counter32(u32::try_from(ctx.metrics.frames_dropped.get()).unwrap_or(u32::MAX))
}
fn tamper_active(ctx: &SnmpContext) -> Value {
    Value::Gauge32(u32::try_from(ctx.metrics.tamper_active.get().max(0)).unwrap_or(0))
}
fn storage_free_mb(ctx: &SnmpContext) -> Value {
    Value::Gauge32(u32::try_from(ctx.metrics.storage_free_mb.get().max(0)).unwrap_or(0))
}
fn storage_used_mb(ctx: &SnmpContext) -> Value {
    Value::Gauge32(u32::try_from(ctx.metrics.storage_used_mb.get().max(0)).unwrap_or(0))
}
// `telemetry_enabled` would collide with the SnmpContext field of the same
// name if this were named identically at module scope.
fn telemetry_enabled_value(ctx: &SnmpContext) -> Value {
    Value::Integer(i64::from(ctx.telemetry_enabled))
}

struct Request {
    community: Vec<u8>,
    pdu_type: u8,
    request_id: i64,
    oids: Vec<Vec<u32>>,
}

fn parse_message(data: &[u8]) -> Option<Request> {
    let (outer, trailing) = ber::read_tlv(data)?;
    if outer.tag != ber::TAG_SEQUENCE || !trailing.is_empty() {
        return None;
    }
    let (_version_tlv, after_version) = ber::read_tlv(outer.content)?;
    let (community_tlv, after_community) = ber::read_tlv(after_version)?;
    let (pdu_tlv, after_pdu) = ber::read_tlv(after_community)?;
    if !after_pdu.is_empty() {
        return None;
    }

    let (request_id_tlv, after_reqid) = ber::read_tlv(pdu_tlv.content)?;
    let request_id = ber::decode_integer(request_id_tlv.content)?;
    let (_error_status, after_err) = ber::read_tlv(after_reqid)?;
    let (_error_index, after_erridx) = ber::read_tlv(after_err)?;
    let (varbind_list, after_vbl) = ber::read_tlv(after_erridx)?;
    if varbind_list.tag != ber::TAG_SEQUENCE || !after_vbl.is_empty() {
        return None;
    }

    let mut oids = Vec::new();
    let mut remaining = varbind_list.content;
    while !remaining.is_empty() {
        let (varbind, rest) = ber::read_tlv(remaining)?;
        if varbind.tag != ber::TAG_SEQUENCE {
            return None;
        }
        let (name_tlv, _value_tlv) = ber::read_tlv(varbind.content)?;
        if name_tlv.tag != ber::TAG_OID {
            return None;
        }
        oids.push(ber::decode_oid(name_tlv.content)?);
        remaining = rest;
    }

    Some(Request {
        community: community_tlv.content.to_vec(),
        pdu_type: pdu_tlv.tag,
        request_id,
        oids,
    })
}

fn build_message(
    community: &[u8],
    pdu_tag: u8,
    request_id: i64,
    varbinds: &[(Vec<u32>, Value)],
) -> Vec<u8> {
    let vb_entries: Vec<Vec<u8>> = varbinds
        .iter()
        .map(|(oid, value)| ber::encode_sequence(&[ber::encode_oid(oid), value.encode()]))
        .collect();
    let varbind_list = ber::encode_sequence(&vb_entries);

    let mut pdu_content = Vec::new();
    pdu_content.extend(ber::encode_integer(request_id));
    pdu_content.extend(ber::encode_integer(0)); // error-status: noError (v2c signals exceptions per-varbind)
    pdu_content.extend(ber::encode_integer(0)); // error-index
    pdu_content.extend(varbind_list);
    let pdu = ber::tlv(pdu_tag, &pdu_content);

    let mut message_content = Vec::new();
    message_content.extend(ber::encode_integer(1)); // SNMP version: 1 == v2c
    message_content.extend(ber::encode_octet_string(community));
    message_content.extend(pdu);
    ber::tlv(ber::TAG_SEQUENCE, &message_content)
}

fn handle_get(oids: &[Vec<u32>], ctx: &SnmpContext) -> Vec<(Vec<u32>, Value)> {
    let table = oid_table();
    oids.iter()
        .map(
            |oid| match table.iter().find(|(t_oid, _)| *t_oid == oid.as_slice()) {
                Some((_, extract)) => (oid.clone(), extract(ctx)),
                None => (oid.clone(), Value::NoSuchObject),
            },
        )
        .collect()
}

fn handle_get_next(oids: &[Vec<u32>], ctx: &SnmpContext) -> Vec<(Vec<u32>, Value)> {
    let table = oid_table();
    oids.iter()
        .map(
            |oid| match table.iter().find(|(t_oid, _)| **t_oid > *oid.as_slice()) {
                Some((t_oid, extract)) => (t_oid.to_vec(), extract(ctx)),
                None => {
                    let last = table.last().map(|(o, _)| o.to_vec()).unwrap_or_default();
                    (last, Value::EndOfMibView)
                }
            },
        )
        .collect()
}

/// Starts the UDP listener thread. Failures (bad port, no permission for
/// <1024 without CAP_NET_BIND_SERVICE) are logged, never fatal — a camera
/// that can't be SNMP-polled must still stream.
pub fn spawn(cfg: SnmpConfig, ctx: SnmpContext) {
    if !cfg.enabled {
        info!("SNMP agent disabled by config");
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("snmp_agent".to_string())
        .spawn(move || {
            let socket = match UdpSocket::bind(("0.0.0.0", cfg.port)) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "SNMP agent disabled: cannot bind port {} ({e}) -- ports under 1024 need \
                     CAP_NET_BIND_SERVICE (see deploy/fusion-firmware.service) or root",
                        cfg.port
                    );
                    return;
                }
            };
            info!(port = cfg.port, "SNMP agent listening (v2c)");

            let mut buf = [0u8; 1500]; // SNMP over UDP comfortably fits one Ethernet MTU
            loop {
                let (len, src) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("SNMP recv error: {e}");
                        continue;
                    }
                };
                let Some(request) = parse_message(&buf[..len]) else {
                    debug!("SNMP: dropped malformed packet from {src}");
                    continue;
                };
                if request.community != cfg.community.as_bytes() {
                    // Silent drop on wrong community (RFC 3584 / standard agent
                    // behavior) -- responding at all would confirm the agent
                    // exists to a probe that doesn't know the real community.
                    debug!("SNMP: wrong community from {src}");
                    continue;
                }

                let response_varbinds = match request.pdu_type {
                    ber::PDU_GET_REQUEST => handle_get(&request.oids, &ctx),
                    ber::PDU_GET_NEXT_REQUEST => handle_get_next(&request.oids, &ctx),
                    ber::PDU_SET_REQUEST => {
                        debug!("SNMP: SetRequest from {src} rejected (read-only agent)");
                        continue;
                    }
                    other => {
                        debug!("SNMP: unsupported PDU type 0x{other:02X} from {src}");
                        continue;
                    }
                };

                let response = build_message(
                    cfg.community.as_bytes(),
                    ber::PDU_GET_RESPONSE,
                    request.request_id,
                    &response_varbinds,
                );
                if let Err(e) = socket.send_to(&response, src) {
                    warn!("SNMP send error: {e}");
                }
            }
        });
    if let Err(e) = spawned {
        warn!("Failed to spawn SNMP agent: {e}");
    }
}

/// `enterprise.0.N` trap OID convention (RFC 2578 NOTIFICATION-TYPE — the
/// ".0" node is the conventional home for notifications, mirroring how
/// v1's enterprise+specific-trap-number pair worked). `extra_varbinds` are
/// appended after the mandatory sysUpTime/snmpTrapOID pair every v2c trap
/// must lead with.
pub(crate) fn send_trap(
    cfg: &SnmpConfig,
    ctx: &SnmpContext,
    trap_specific_oid: u32,
    extra_varbinds: Vec<(Vec<u32>, Value)>,
) {
    if cfg.trap_host.is_empty() {
        return;
    }
    let mut trap_oid = ENTERPRISE_OID.to_vec();
    trap_oid.extend([0, trap_specific_oid]);

    let mut varbinds = vec![
        (vec![1, 3, 6, 1, 2, 1, 1, 3, 0], sys_up_time(ctx)),
        (vec![1, 3, 6, 1, 6, 3, 1, 1, 4, 1, 0], Value::Oid(trap_oid)),
    ];
    varbinds.extend(extra_varbinds);

    let message = build_message(cfg.community.as_bytes(), ber::PDU_TRAP_V2, 1, &varbinds);
    match UdpSocket::bind("0.0.0.0:0") {
        Ok(socket) => {
            if let Err(e) = socket.send_to(&message, (cfg.trap_host.as_str(), cfg.trap_port)) {
                warn!("SNMP trap send failed: {e}");
            }
        }
        Err(e) => warn!("SNMP trap: could not bind local socket: {e}"),
    }
}

/// `enterprise.0.1` — tamper alarm going active. Passed to `send_trap` from
/// wherever a tamper alarm edge is already detected (main.rs, alongside the
/// existing `tamper_event` telemetry publish).
pub const TAMPER_ALARM_TRAP: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> SnmpContext {
        SnmpContext {
            metrics: Metrics::new(),
            footprint: Arc::new(Footprint {
                device_id: "cam-test-01".to_string(),
                facility_id: "test-facility".to_string(),
                model: "Fusion Test Node".to_string(),
                hardware_id: "test-hw".to_string(),
                firmware_version: "9.9.9",
                git_hash: "deadbeef0000",
                config_hash: "irrelevant".to_string(),
                features: vec![],
            }),
            booted: Instant::now(),
            sys_contact: "ops@example.com".to_string(),
            sys_location: "Test Site".to_string(),
            telemetry_enabled: true,
        }
    }

    fn get_request(request_id: i64, oids: &[&[u32]], community: &str) -> Vec<u8> {
        let varbinds: Vec<Vec<u8>> = oids
            .iter()
            .map(|oid| ber::encode_sequence(&[ber::encode_oid(oid), ber::encode_null()]))
            .collect();
        let varbind_list = ber::encode_sequence(&varbinds);
        let mut pdu_content = Vec::new();
        pdu_content.extend(ber::encode_integer(request_id));
        pdu_content.extend(ber::encode_integer(0));
        pdu_content.extend(ber::encode_integer(0));
        pdu_content.extend(varbind_list);
        let pdu = ber::tlv(ber::PDU_GET_REQUEST, &pdu_content);

        let mut message_content = Vec::new();
        message_content.extend(ber::encode_integer(1));
        message_content.extend(ber::encode_octet_string(community.as_bytes()));
        message_content.extend(pdu);
        ber::tlv(ber::TAG_SEQUENCE, &message_content)
    }

    #[test]
    fn parses_a_real_get_request() {
        let packet = get_request(42, &[&[1, 3, 6, 1, 2, 1, 1, 1, 0]], "public");
        let req = parse_message(&packet).expect("valid packet must parse");
        assert_eq!(req.request_id, 42);
        assert_eq!(req.community, b"public");
        assert_eq!(req.pdu_type, ber::PDU_GET_REQUEST);
        assert_eq!(req.oids, vec![vec![1, 3, 6, 1, 2, 1, 1, 1, 0]]);
    }

    #[test]
    fn malformed_packets_never_panic() {
        for junk in [&[][..], &[0xFF][..], &[0x30, 0x05, 0x01, 0x02][..]] {
            assert!(parse_message(junk).is_none());
        }
    }

    #[test]
    fn get_on_known_oid_returns_the_real_value() {
        let context = ctx();
        let results = handle_get(&[vec![1, 3, 6, 1, 2, 1, 1, 5, 0]], &context);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, Value::OctetString(b"cam-test-01".to_vec()));
    }

    #[test]
    fn get_on_unknown_oid_returns_no_such_object() {
        let context = ctx();
        let results = handle_get(&[vec![9, 9, 9]], &context);
        assert_eq!(results[0].1, Value::NoSuchObject);
    }

    #[test]
    fn get_next_walks_in_strictly_ascending_oid_order() {
        let context = ctx();
        let table = oid_table();
        let mut current = vec![1u32]; // before every real entry
        for (expected_oid, _) in table.iter() {
            let next = handle_get_next(&[current.clone()], &context);
            assert_eq!(next[0].0, expected_oid.to_vec());
            current = next[0].0.clone();
        }
        // Walking past the last entry hits end-of-MIB, not a panic or a
        // repeat of the last entry.
        let past_end = handle_get_next(&[current], &context);
        assert_eq!(past_end[0].1, Value::EndOfMibView);
    }

    #[test]
    fn response_round_trips_through_the_same_ber_reader_a_real_client_uses() {
        let context = ctx();
        let varbinds = handle_get(&[vec![1, 3, 6, 1, 2, 1, 1, 3, 0]], &context);
        let message = build_message(b"public", ber::PDU_GET_RESPONSE, 7, &varbinds);

        let (outer, trailing) = ber::read_tlv(&message).unwrap();
        assert!(trailing.is_empty());
        assert_eq!(outer.tag, ber::TAG_SEQUENCE);
        let (_version, after_version) = ber::read_tlv(outer.content).unwrap();
        let (community, after_community) = ber::read_tlv(after_version).unwrap();
        assert_eq!(community.content, b"public");
        let (pdu, _) = ber::read_tlv(after_community).unwrap();
        assert_eq!(pdu.tag, ber::PDU_GET_RESPONSE);
    }

    /// Real interop check against actual net-snmp tooling, not just this
    /// module's own encoder read back by its own decoder. `#[ignore]`d:
    /// needs `snmpget`/`snmpwalk` on PATH, which CI doesn't install. Run
    /// manually: `cargo test snmp:: -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_snmpwalk_agrees_with_this_agent() {
        use std::process::Command;

        let port = 11611;
        let cfg = SnmpConfig {
            enabled: true,
            port,
            community: "public".to_string(),
            ..SnmpConfig::default()
        };
        spawn(cfg, ctx());
        std::thread::sleep(std::time::Duration::from_millis(200));

        let out = Command::new("snmpget")
            .args([
                "-v2c",
                "-c",
                "public",
                "-Ot",
                &format!("127.0.0.1:{port}"),
                "1.3.6.1.2.1.1.5.0", // sysName
            ])
            .output()
            .expect("snmpget must be installed for this test");
        let stdout = String::from_utf8_lossy(&out.stdout);
        println!("snmpget sysName: {stdout}");
        assert!(
            stdout.contains("cam-test-01"),
            "unexpected snmpget output: {stdout}"
        );

        let walk = Command::new("snmpwalk")
            .args([
                "-v2c",
                "-c",
                "public",
                &format!("127.0.0.1:{port}"),
                "1.3.6.1",
            ])
            .output()
            .expect("snmpwalk must be installed for this test");
        let walk_out = String::from_utf8_lossy(&walk.stdout);
        println!("snmpwalk output:\n{walk_out}");
        // Every OID this agent serves should show up in a full walk, and
        // net-snmp itself must not report a parse/protocol error.
        assert!(
            !walk_out.to_lowercase().contains("error"),
            "snmpwalk reported an error: {walk_out}"
        );
        for label in ["STRING: cam-test-01", "Timeticks", "Counter32", "Gauge32"] {
            assert!(
                walk_out.contains(label),
                "expected '{label}' in walk output:\n{walk_out}"
            );
        }
    }
}
