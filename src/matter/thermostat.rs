// src/matter/thermostat.rs
//
// Thermostat cluster (Matter cluster 0x0201), built directly against
// rs-matter's lower-level, public `Handler` trait — the same primitive
// on_off/level/color are themselves implemented on top of.
//
// CORRECTION (2026-10-01): this file originally claimed the crate had no
// existing implementation for this cluster at all. That was wrong. rs-matter
// 0.3.0 has no ready-made *application handler* (hooks + spec-rule
// enforcement) for Thermostat — only on_off/level_control/color_control and
// the camera clusters do — but it DOES ship a typed, spec-generated
// declaration for it (`rs_matter::dm::clusters::decl::thermostat`: a
// per-attribute `ClusterHandler` trait, `FULL_CLUSTER` metadata, typed
// builders). The attribute/command IDs hand-written below were cross-checked
// against that generated data and match. The right long-term shape is to
// implement that typed trait (spec-correct types for free) — or adopt the
// upstream Pattern-B1 `ThermostatHooks` handler once it ships in a release —
// instead of raw TLV; planned in TODO.md Phase 19g.
//
// Verified against the installed rs-matter 0.3.0 source before writing a
// line of this file (no guessing): `Handler::read/write/invoke/
// bump_dataver`, `AttrDetails`/`CmdDetails` field shapes, `Cluster::new`/
// `Attribute::new`/`Command::new` (hand-constructed here — there is no
// `FULL_CLUSTER` decl constant to start from and filter, unlike every
// other cluster in src/matter/), `ReadReply::with_dataver` -> `Reply::set`,
// `InvokeReply`, global attributes (ClusterRevision/FeatureMap/...)
// confirmed handled generically by the framework from `Cluster` metadata
// (src/dm/types/attribute.rs) and never reaching a cluster's own `read()`.
// Multi-field command decoding (`TLVElement::structure()?.ctx(n)?` +
// `FromTLV::from_tlv`) mirrors the one real hand-decoded command payload
// anywhere in this crate's own source, `dm::clusters::time_sync`'s
// `SetTimeZone`.
//
// CONFIDENCE NOTE, stated plainly: this cluster has no crate-provided
// `validate()` safety net (on_off/level_control/color_control panic at
// startup on a cluster-metadata mistake; a hand-rolled cluster like this
// one has no such check — a mistake here would surface only against a
// real controller's read/write, not at boot). The cluster/attribute/
// command ID numbers below are the Thermostat cluster's long-stable base-
// spec values, consistent across every public Matter SDK/sample this
// project has been built against so far — but they are NOT independently
// checked against the official CSA Matter spec PDF (no copy of it exists
// in this project). Treat this as mechanically-sound and live-tested
// against this crate's own IM dispatch, but not yet carrying the same
// multi-controller confidence as on_off/level_control/color_control until
// it has had a real controller (Apple Home / chip-tool) read+write pass.
//
// Hardware reality: this board has no HVAC equipment at all (no
// compressor, no heating element, no real thermostat relay) — `SystemMode`
// and both setpoints are honestly in-memory only, exactly like
// `light::InMemoryLevelHooks`/`InMemoryColorHooks` before any real
// dimmer/RGB driver exists. The one REAL signal is `LocalTemperature`,
// backed by the SoC's own thermal-zone reading (`health::read_soc_temp_c`,
// already a Prometheus gauge) — this is DEVICE/BOARD temperature, not
// room-ambient, and `Nullable::none()` (not a fabricated 0°C) is reported
// whenever that read fails, same "never fake a reading you don't have"
// rule the radar/LiDAR modules already follow.
use std::sync::atomic::{AtomicI16, AtomicU8, Ordering};

use rs_matter::dm::{
    Access, Async, AttrId, Attribute, Cluster, ClusterId, Command, Dataver, EndptId, InvokeContext,
    InvokeReply, MatchContext, NonBlockingHandler, Quality, ReadContext,
    ReadReply, Reply, WriteContext,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::{FromTLV, Nullable};

use crate::health::read_soc_temp_c;
use crate::matter::registry::{ClusterImpl, EndpointSpec};

use tracing::info;

pub(crate) const THERMOSTAT_ENDPOINT_ID: EndptId = 4;

const CLUSTER_ID_THERMOSTAT: ClusterId = 0x0201;

mod attr_id {
    use rs_matter::dm::AttrId;
    pub const LOCAL_TEMPERATURE: AttrId = 0x0000;
    pub const OCCUPIED_COOLING_SETPOINT: AttrId = 0x0011;
    pub const OCCUPIED_HEATING_SETPOINT: AttrId = 0x0012;
    pub const CONTROL_SEQUENCE_OF_OPERATION: AttrId = 0x001B;
    pub const SYSTEM_MODE: AttrId = 0x001C;
}

mod cmd_id {
    use rs_matter::dm::CmdId;
    pub const SETPOINT_RAISE_LOWER: CmdId = 0x0000;
}

/// `SystemMode` values this implementation accepts/reports (Matter spec
/// `ThermostatSystemModeEnum`; only the universally-supported subset).
mod system_mode {
    pub const OFF: u8 = 0;
    pub const AUTO: u8 = 1;
    pub const COOL: u8 = 3;
    pub const HEAT: u8 = 4;
}

/// `ControlSequenceOfOperation` reported value — fixed, honest about there
/// being no real equipment: "CoolingAndHeating" (4) is the most generic,
/// least-committal value (neither claims cooling-only nor heating-only
/// capability a real controller might otherwise gate UI on).
const CONTROL_SEQUENCE_COOLING_AND_HEATING: u8 = 4;

pub(crate) const CLUSTER: Cluster<'static> = Cluster::new(
    CLUSTER_ID_THERMOSTAT,
    6,
    0,
    &[
        Attribute::new(attr_id::LOCAL_TEMPERATURE, Access::RV, Quality::NULLABLE),
        Attribute::new(
            attr_id::OCCUPIED_COOLING_SETPOINT,
            Access::RWVM,
            Quality::NONE,
        ),
        Attribute::new(
            attr_id::OCCUPIED_HEATING_SETPOINT,
            Access::RWVM,
            Quality::NONE,
        ),
        Attribute::new(
            attr_id::CONTROL_SEQUENCE_OF_OPERATION,
            Access::RV,
            Quality::NONE,
        ),
        Attribute::new(attr_id::SYSTEM_MODE, Access::RWVM, Quality::NONE),
    ],
    &[Command::new(cmd_id::SETPOINT_RAISE_LOWER, None, Access::WM)],
    &[],
    |_, _, _| true,
    |_, _, _| true,
    |_, _, _| true,
);

/// In-memory thermostat state. `LocalTemperature` is NOT stored here — it's
/// read live from `health::read_soc_temp_c()` on every read, same as any
/// other live sensor reading in this firmware (never cached/stale).
pub(crate) struct ThermostatHandler {
    dataver: Dataver,
    system_mode: AtomicU8,
    occupied_cooling_setpoint_centidegrees: AtomicI16,
    occupied_heating_setpoint_centidegrees: AtomicI16,
}

impl ThermostatHandler {
    fn new(dataver: Dataver) -> Self {
        Self {
            dataver,
            system_mode: AtomicU8::new(system_mode::OFF),
            // 24.00C / 20.00C: unremarkable, widely-used defaults for a
            // virtual thermostat with no real setpoint behind it.
            occupied_cooling_setpoint_centidegrees: AtomicI16::new(2400),
            occupied_heating_setpoint_centidegrees: AtomicI16::new(2000),
        }
    }
}

impl NonBlockingHandler for ThermostatHandler {}

impl rs_matter::dm::Handler for ThermostatHandler {
    fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let Some(r) = reply.with_dataver(self.dataver.get())? else {
            return Ok(());
        };

        match ctx.attr().attr_id {
            attr_id::LOCAL_TEMPERATURE => {
                let value = match read_soc_temp_c() {
                    Some(c) => Nullable::some((c * 100.0).round() as i16),
                    None => Nullable::none(),
                };
                r.set(value)
            }
            attr_id::OCCUPIED_COOLING_SETPOINT => r.set(
                self.occupied_cooling_setpoint_centidegrees
                    .load(Ordering::Relaxed),
            ),
            attr_id::OCCUPIED_HEATING_SETPOINT => r.set(
                self.occupied_heating_setpoint_centidegrees
                    .load(Ordering::Relaxed),
            ),
            attr_id::CONTROL_SEQUENCE_OF_OPERATION => r.set(CONTROL_SEQUENCE_COOLING_AND_HEATING),
            attr_id::SYSTEM_MODE => r.set(self.system_mode.load(Ordering::Relaxed)),
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        match ctx.attr().attr_id {
            attr_id::OCCUPIED_COOLING_SETPOINT => {
                let v = i16::from_tlv(ctx.data())?;
                self.occupied_cooling_setpoint_centidegrees
                    .store(v, Ordering::Relaxed);
                ctx.notify_changed();
                Ok(())
            }
            attr_id::OCCUPIED_HEATING_SETPOINT => {
                let v = i16::from_tlv(ctx.data())?;
                self.occupied_heating_setpoint_centidegrees
                    .store(v, Ordering::Relaxed);
                ctx.notify_changed();
                Ok(())
            }
            attr_id::SYSTEM_MODE => {
                let v = u8::from_tlv(ctx.data())?;
                if !matches!(
                    v,
                    system_mode::OFF | system_mode::AUTO | system_mode::COOL | system_mode::HEAT
                ) {
                    return Err(ErrorCode::ConstraintError.into());
                }
                info!(mode = v, "Matter thermostat: system mode set (no real HVAC equipment to actually switch)");
                self.system_mode.store(v, Ordering::Relaxed);
                ctx.notify_changed();
                Ok(())
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    fn invoke(&self, ctx: impl InvokeContext, _reply: impl InvokeReply) -> Result<(), Error> {
        match ctx.cmd().cmd_id {
            cmd_id::SETPOINT_RAISE_LOWER => {
                let s = ctx.data().structure()?;
                let mode = u8::from_tlv(&s.ctx(0)?)?;
                // `Amount`: int8, in steps of 0.1 degC per spec.
                let amount = i8::from_tlv(&s.ctx(1)?)?;
                let delta = i16::from(amount) * 10;

                let adjust = |setpoint: &AtomicI16, attr: AttrId| {
                    setpoint.fetch_add(delta, Ordering::Relaxed);
                    ctx.notify_own_attr_changed(attr);
                };

                match mode {
                    0 => adjust(
                        &self.occupied_heating_setpoint_centidegrees,
                        attr_id::OCCUPIED_HEATING_SETPOINT,
                    ),
                    1 => adjust(
                        &self.occupied_cooling_setpoint_centidegrees,
                        attr_id::OCCUPIED_COOLING_SETPOINT,
                    ),
                    2 => {
                        adjust(
                            &self.occupied_heating_setpoint_centidegrees,
                            attr_id::OCCUPIED_HEATING_SETPOINT,
                        );
                        adjust(
                            &self.occupied_cooling_setpoint_centidegrees,
                            attr_id::OCCUPIED_COOLING_SETPOINT,
                        );
                    }
                    _ => return Err(ErrorCode::InvalidCommand.into()),
                }

                Ok(())
            }
            _ => Err(ErrorCode::CommandNotFound.into()),
        }
    }

    fn bump_dataver(&self, ctx: impl MatchContext) {
        if ctx.cluster() == Some(CLUSTER_ID_THERMOSTAT) {
            self.dataver.changed();
        }
    }
}

pub(crate) fn build(rand: &mut impl rand_core::Rng) -> &'static ThermostatHandler {
    Box::leak(Box::new(ThermostatHandler::new(Dataver::new_rand(rand))))
}

/// This device as a registry endpoint (fixed endpoint id 4). The Descriptor
/// cluster is added by the registry.
pub(crate) fn spec(thermostat: &'static ThermostatHandler) -> EndpointSpec {
    EndpointSpec {
        id: Some(THERMOSTAT_ENDPOINT_ID),
        name: "thermostat".to_string(),
        device_types: vec![rs_matter::dm::DeviceType {
            dtype: 0x0301,
            drev: 1,
        }],
        clusters: vec![(CLUSTER, ClusterImpl::Thermostat(Async(thermostat)))],
    }
}
