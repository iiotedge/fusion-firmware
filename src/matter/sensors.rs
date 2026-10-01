// src/matter/sensors.rs
//
// Config-driven Matter sensors (Phase 19g.2): `[[matter.endpoints]]` entries of
// kind temperature / humidity / pressure / flow / illuminance / occupancy /
// contact, each bound by a `source` spec to whatever produces the value (see
// src/signals.rs). One firmware binary becomes any of these purely by config.
//
// BUILT ON rs-matter's TYPED cluster layer (`dm::clusters::decl::*`): each
// cluster is a handful of getters on a generated trait, with attribute ids,
// types and metadata coming from the real Matter spec data — nothing here
// hand-encodes TLV or hard-codes an attribute id (unlike the older raw-`Handler`
// Thermostat, which is slated to move onto this layer).
//
// HONESTY RULES (the same ones the radar/LiDAR modules follow):
//   * No reading -> Matter `null` for nullable measurements, an explicit error
//     status for non-nullable attributes. Never a made-up 0 / "unoccupied".
//   * A reading outside the sensor's declared physical range -> `null`, not
//     trusted.
//   * A source marked synthetic (mock camera/radar) is refused unless the
//     endpoint sets `allow_mock = true`, so a real controller is never shown
//     fake data as if it were real.
//
// LIVE UPDATES. Each handler's background `run()` samples its source every
// `poll_ms` and, when the Matter-visible value changes, notifies subscribers —
// that is what makes a controller's UI update without polling. Sources stay
// pull-based, so producers (the analytics thread, a sysfs file, an HTTP push)
// need no Matter-specific code.
//
// NOT YET DONE (tracked in TODO.md 19g.2): the optional change *events*
// (`StateChange`, `OccupancyChanged`) and Generic Switch (press events);
// attribute change reporting is complete, events are an additional channel.
use core::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rs_matter::dm::clusters::decl::{
    boolean_state, flow_measurement, illuminance_measurement, occupancy_sensing,
    pressure_measurement, relative_humidity_measurement, temperature_measurement,
};
use rs_matter::dm::{
    Async, AttrId, Cluster, ClusterId, Dataver, DeviceType, EndptId,
    HandlerContext, ReadContext,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::tlv::Nullable;
use rs_matter::with;

use crate::config::{effective_endpoint_name, effective_poll_ms, MatterEndpointConfig, MatterEndpointKind};
use crate::matter::registry::{identify_cluster, ClusterImpl, EndpointSpec};
use crate::signals::{parse_spec, Provenance, SignalBus, Source};

/// Attribute id of `MeasuredValue` / `Occupancy` / `StateValue` — id 0 in every
/// cluster used here (verified against the generated spec data).
const ATTR_PRIMARY: AttrId = 0;

/// State shared by every sensor handler.
struct Common {
    endpoint: EndptId,
    source: Arc<dyn Source>,
    poll: Duration,
    dataver: Dataver,
    scale: f64,
    offset: f64,
    /// Physical range in the kind's natural unit; a reading outside it is "no
    /// reading".
    nat_min: f64,
    nat_max: f64,
    invert: bool,
}

impl Common {
    /// The current numeric reading in the kind's natural unit (after
    /// scale/offset), or `None` if there's no reading or it is outside the
    /// sensor's declared range.
    fn numeric(&self) -> Option<f64> {
        let raw = self.source.read()?.value.as_f64();
        let v = raw * self.scale + self.offset;
        (v.is_finite() && v >= self.nat_min && v <= self.nat_max).then_some(v)
    }

    /// The current boolean reading (after `invert`), or `None`.
    fn boolean(&self) -> Option<bool> {
        self.source.read().map(|r| r.value.as_bool() != self.invert)
    }

    /// Sample every `poll`; when the Matter-visible value changes, tell
    /// subscribers. Never returns on its own.
    async fn watch<C, T>(
        &self,
        ctx: C,
        cluster: ClusterId,
        mut sample: impl FnMut() -> T,
    ) -> Result<(), Error>
    where
        C: HandlerContext,
        T: PartialEq,
    {
        let mut last = sample();
        loop {
            async_io::Timer::after(self.poll).await;
            let now = sample();
            if now != last {
                last = now;
                ctx.notify_attr_changed(self.endpoint, cluster, ATTR_PRIMARY);
            }
        }
    }
}

/// How a natural-unit value maps onto the cluster's raw integer.
#[derive(Clone, Copy)]
struct Units {
    to_raw: fn(f64) -> i64,
    default_min: f64,
    default_max: f64,
    /// The raw type's valid range (excludes the type's null sentinel).
    raw_min: i64,
    raw_max: i64,
    /// Illuminance only: raw 0 means "too dark to measure" and is a valid
    /// reading even though it is below `MinMeasuredValue`.
    zero_is_valid: bool,
}

const TEMPERATURE: Units = Units {
    // 0.01 degC, int16
    to_raw: |c| (c * 100.0).round() as i64,
    default_min: -40.0,
    default_max: 125.0,
    raw_min: -27315,
    raw_max: 32767,
    zero_is_valid: false,
};
const HUMIDITY: Units = Units {
    // 0.01 %, uint16
    to_raw: |p| (p * 100.0).round() as i64,
    default_min: 0.0,
    default_max: 100.0,
    raw_min: 0,
    raw_max: 10000,
    zero_is_valid: false,
};
const PRESSURE: Units = Units {
    // 0.1 kPa == 1 hPa, int16
    to_raw: |hpa| hpa.round() as i64,
    default_min: 300.0,
    default_max: 1100.0,
    raw_min: -32767,
    raw_max: 32767,
    zero_is_valid: false,
};
const ILLUMINANCE: Units = Units {
    // 10000 * log10(lux) + 1, uint16; < 1 lux is raw 0 ("too dark to measure")
    to_raw: |lux| {
        if lux < 1.0 {
            0
        } else {
            (10000.0 * lux.log10() + 1.0).round() as i64
        }
    },
    default_min: 1.0,
    default_max: 100_000.0,
    raw_min: 1,
    raw_max: 0xFFFE,
    zero_is_valid: true,
};
const FLOW: Units = Units {
    // 0.1 m3/h, uint16
    to_raw: |m3h| (m3h * 10.0).round() as i64,
    default_min: 0.0,
    default_max: 1000.0,
    raw_min: 0,
    raw_max: 0xFFFE,
    zero_is_valid: false,
};

fn clamp_raw(raw: i64, units: &Units) -> i64 {
    raw.clamp(units.raw_min, units.raw_max)
}

/// Generates one measurement-cluster handler. `$raw` is the cluster's integer
/// type; `$ctor_cluster` its cluster id (verified against the generated spec).
macro_rules! measurement_cluster {
    ($handler:ident, $module:ident, $cluster_id:expr, $raw:ty, $units:expr) => {
        pub(crate) struct $handler {
            common: Common,
            units: Units,
            min_raw: i64,
            max_raw: i64,
        }

        impl $handler {
            fn new(common: Common, units: Units) -> Self {
                let min_raw = clamp_raw((units.to_raw)(common.nat_min), &units);
                let max_raw = clamp_raw((units.to_raw)(common.nat_max), &units);
                Self {
                    common,
                    units,
                    min_raw,
                    max_raw,
                }
            }

            /// Matter-visible `MeasuredValue`: `None` = null.
            fn sample(&self) -> Option<$raw> {
                let nat = self.common.numeric()?;
                let raw = (self.units.to_raw)(nat);
                let in_range = raw >= self.min_raw && raw <= self.max_raw;
                (in_range || (self.units.zero_is_valid && raw == 0)).then(|| raw as $raw)
            }
        }

        impl $module::ClusterHandler for $handler {
            const CLUSTER: Cluster<'static> =
                $module::FULL_CLUSTER.with_attrs(with!(required)).with_cmds(with!());

            fn dataver(&self) -> u32 {
                self.common.dataver.get()
            }

            fn dataver_changed(&self) {
                self.common.dataver.changed();
            }

            fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<$raw>, Error> {
                Ok(match self.sample() {
                    Some(v) => Nullable::some(v),
                    None => Nullable::none(),
                })
            }

            fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<$raw>, Error> {
                Ok(Nullable::some(self.min_raw as $raw))
            }

            fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<$raw>, Error> {
                Ok(Nullable::some(self.max_raw as $raw))
            }

            fn run(
                &self,
                ctx: impl HandlerContext,
            ) -> impl Future<Output = Result<(), Error>> {
                self.common.watch(ctx, $cluster_id, || self.sample())
            }
        }
    };
}

measurement_cluster!(TemperatureHandler, temperature_measurement, 0x0402, i16, TEMPERATURE);
measurement_cluster!(HumidityHandler, relative_humidity_measurement, 0x0405, u16, HUMIDITY);
measurement_cluster!(PressureHandler, pressure_measurement, 0x0403, i16, PRESSURE);
measurement_cluster!(IlluminanceHandler, illuminance_measurement, 0x0400, u16, ILLUMINANCE);
measurement_cluster!(FlowHandler, flow_measurement, 0x0404, u16, FLOW);

/// Contact sensor: `StateValue` true = closed/contact (Matter's Contact Sensor
/// meaning); use `invert` for a normally-closed circuit.
pub(crate) struct BooleanStateHandler {
    common: Common,
}

impl boolean_state::ClusterHandler for BooleanStateHandler {
    const CLUSTER: Cluster<'static> = boolean_state::FULL_CLUSTER
        .with_attrs(with!(required))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.common.dataver.get()
    }

    fn dataver_changed(&self) {
        self.common.dataver.changed();
    }

    fn state_value(&self, _ctx: impl ReadContext) -> Result<bool, Error> {
        // StateValue is non-nullable: with no reading, say so rather than
        // guess open/closed.
        self.common.boolean().ok_or_else(|| ErrorCode::Failure.into())
    }

    fn run(&self, ctx: impl HandlerContext) -> impl Future<Output = Result<(), Error>> {
        self.common.watch(ctx, 0x0045, || self.common.boolean())
    }
}

/// What kind of sensing technology an occupancy endpoint claims (Matter 1.5
/// added RADAR / VISION / ... beside the legacy PIR / ULTRASONIC / PHYSICAL_CONTACT).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OccupancyTech {
    Pir,
    Ultrasonic,
    PhysicalContact,
    Vision,
    Radar,
    Other,
}

impl OccupancyTech {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "" | "pir" => Self::Pir,
            "ultrasonic" => Self::Ultrasonic,
            "physical_contact" => Self::PhysicalContact,
            "vision" => Self::Vision,
            "radar" => Self::Radar,
            "other" => Self::Other,
            _ => return None,
        })
    }

    fn feature(self) -> occupancy_sensing::Feature {
        use occupancy_sensing::Feature as F;
        match self {
            Self::Pir => F::PASSIVE_INFRARED,
            Self::Ultrasonic => F::ULTRASONIC,
            Self::PhysicalContact => F::PHYSICAL_CONTACT,
            Self::Vision => F::VISION,
            Self::Radar => F::RADAR,
            Self::Other => F::OTHER,
        }
    }

    /// The legacy `OccupancySensorType` (Mandatory-but-Deprecated in the 1.5
    /// spec) only knows three technologies. For the newer ones the closest
    /// legacy value is PIR; the real technology is carried by the feature bit,
    /// which is what current controllers read.
    fn legacy_type(self) -> occupancy_sensing::OccupancySensorTypeEnum {
        use occupancy_sensing::OccupancySensorTypeEnum as T;
        match self {
            Self::Ultrasonic => T::Ultrasonic,
            Self::PhysicalContact => T::PhysicalContact,
            Self::Pir | Self::Vision | Self::Radar | Self::Other => T::PIR,
        }
    }

    fn legacy_bitmap(self) -> occupancy_sensing::OccupancySensorTypeBitmap {
        use occupancy_sensing::OccupancySensorTypeBitmap as B;
        match self {
            Self::Ultrasonic => B::ULTRASONIC,
            Self::PhysicalContact => B::PHYSICAL_CONTACT,
            Self::Pir | Self::Vision | Self::Radar | Self::Other => B::PIR,
        }
    }
}

pub(crate) struct OccupancyHandler {
    common: Common,
    tech: OccupancyTech,
}

impl OccupancyHandler {
    fn cluster(tech: OccupancyTech) -> Cluster<'static> {
        occupancy_sensing::FULL_CLUSTER
            .with_features(tech.feature().bits())
            .with_attrs(with!(required))
            .with_cmds(with!())
    }
}

impl occupancy_sensing::ClusterHandler for OccupancyHandler {
    // The feature bit depends on the configured technology, which is a runtime
    // value; the per-endpoint `Cluster` is built by `OccupancyHandler::cluster`
    // and handed to the registry. This const is the default (PIR) shape.
    const CLUSTER: Cluster<'static> = occupancy_sensing::FULL_CLUSTER
        .with_features(occupancy_sensing::Feature::PASSIVE_INFRARED.bits())
        .with_attrs(with!(required))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.common.dataver.get()
    }

    fn dataver_changed(&self) {
        self.common.dataver.changed();
    }

    fn occupancy(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancyBitmap, Error> {
        // Non-nullable bitmap: with no reading, say so rather than claim "empty".
        let occupied = self.common.boolean().ok_or(ErrorCode::Failure)?;
        Ok(if occupied {
            occupancy_sensing::OccupancyBitmap::OCCUPIED
        } else {
            occupancy_sensing::OccupancyBitmap::empty()
        })
    }

    fn occupancy_sensor_type(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancySensorTypeEnum, Error> {
        Ok(self.tech.legacy_type())
    }

    fn occupancy_sensor_type_bitmap(
        &self,
        _ctx: impl ReadContext,
    ) -> Result<occupancy_sensing::OccupancySensorTypeBitmap, Error> {
        Ok(self.tech.legacy_bitmap())
    }

    fn run(&self, ctx: impl HandlerContext) -> impl Future<Output = Result<(), Error>> {
        self.common.watch(ctx, 0x0406, || self.common.boolean())
    }
}

fn not_a_sensor(kind: &str) -> String {
    format!("kind '{kind}' is not a sensor")
}

/// Parse and open an endpoint's `source`, refusing a synthetic one (the mock
/// camera/radar) unless the endpoint opts in with `allow_mock` — a real
/// controller must never be shown fake data as if it were real.
pub(crate) fn resolve_source(cfg: &MatterEndpointConfig, bus: &SignalBus) -> Result<Arc<dyn Source>, String> {
    let spec = parse_spec(&cfg.source)?;
    let source = bus.resolve(&spec)?;
    if source.provenance() == Provenance::Mock && !cfg.allow_mock {
        return Err(format!(
            "source '{}' is synthetic (mock camera/radar data) and would show fake readings to a real \
             controller; set allow_mock = true only for bench/dev use",
            cfg.source
        ));
    }
    Ok(source)
}

/// Build one `[[matter.endpoints]]` entry. Fails (so the caller can skip just
/// this endpoint and keep the rest of the node up) on an unusable source.
pub(crate) fn build_endpoint<R: rand_core::Rng>(
    cfg: &MatterEndpointConfig,
    index: usize,
    id: EndptId,
    bus: &SignalBus,
    rand: &mut R,
) -> Result<EndpointSpec, String> {
    let kind = MatterEndpointKind::parse(&cfg.kind)
        .ok_or_else(|| format!("unknown kind '{}'", cfg.kind))?;
    if kind.is_actuator() || kind == MatterEndpointKind::GenericSwitch {
        // Checked before the source is parsed: an actuator has none.
        return Err(not_a_sensor(&cfg.kind));
    }
    let name = effective_endpoint_name(cfg, index);
    let source = resolve_source(cfg, bus)?;

    let units = match kind {
        MatterEndpointKind::Temperature => Some(TEMPERATURE),
        MatterEndpointKind::Humidity => Some(HUMIDITY),
        MatterEndpointKind::Pressure => Some(PRESSURE),
        MatterEndpointKind::Illuminance => Some(ILLUMINANCE),
        MatterEndpointKind::Flow => Some(FLOW),
        MatterEndpointKind::Occupancy | MatterEndpointKind::Contact => None,
        MatterEndpointKind::OnOffLight
        | MatterEndpointKind::OnOffPlug
        | MatterEndpointKind::Fan
        | MatterEndpointKind::GenericSwitch => return Err(not_a_sensor(&cfg.kind)),
    };
    let common = Common {
        endpoint: id,
        source: source.clone(),
        poll: Duration::from_millis(effective_poll_ms(kind, cfg)),
        dataver: Dataver::new_rand(rand),
        scale: cfg.scale,
        offset: cfg.offset,
        nat_min: cfg.min.or(units.map(|u| u.default_min)).unwrap_or(f64::MIN),
        nat_max: cfg.max.or(units.map(|u| u.default_max)).unwrap_or(f64::MAX),
        invert: cfg.invert,
    };

    // (device type, measurement/state cluster metadata, its handler)
    let (device_type, meta, imp) = match kind {
        MatterEndpointKind::Temperature => {
            let h = TemperatureHandler::new(common, TEMPERATURE);
            (
                DeviceType { dtype: 0x0302, drev: 3 },
                <TemperatureHandler as temperature_measurement::ClusterHandler>::CLUSTER,
                ClusterImpl::Temperature(Async(temperature_measurement::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::Humidity => {
            let h = HumidityHandler::new(common, HUMIDITY);
            (
                DeviceType { dtype: 0x0307, drev: 3 },
                <HumidityHandler as relative_humidity_measurement::ClusterHandler>::CLUSTER,
                ClusterImpl::Humidity(Async(relative_humidity_measurement::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::Pressure => {
            let h = PressureHandler::new(common, PRESSURE);
            (
                DeviceType { dtype: 0x0305, drev: 3 },
                <PressureHandler as pressure_measurement::ClusterHandler>::CLUSTER,
                ClusterImpl::Pressure(Async(pressure_measurement::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::Illuminance => {
            let h = IlluminanceHandler::new(common, ILLUMINANCE);
            (
                DeviceType { dtype: 0x0106, drev: 4 },
                <IlluminanceHandler as illuminance_measurement::ClusterHandler>::CLUSTER,
                ClusterImpl::Illuminance(Async(illuminance_measurement::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::Flow => {
            let h = FlowHandler::new(common, FLOW);
            (
                DeviceType { dtype: 0x0306, drev: 3 },
                <FlowHandler as flow_measurement::ClusterHandler>::CLUSTER,
                ClusterImpl::Flow(Async(flow_measurement::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::Contact => {
            let h = BooleanStateHandler { common };
            (
                DeviceType { dtype: 0x0015, drev: 2 },
                <BooleanStateHandler as boolean_state::ClusterHandler>::CLUSTER,
                ClusterImpl::BooleanState(Async(boolean_state::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::Occupancy => {
            let tech = OccupancyTech::parse(&cfg.occupancy_type)
                .ok_or_else(|| format!("unknown occupancy_type '{}'", cfg.occupancy_type))?;
            let h = OccupancyHandler { common, tech };
            (
                DeviceType { dtype: 0x0107, drev: 4 },
                OccupancyHandler::cluster(tech),
                ClusterImpl::Occupancy(Async(occupancy_sensing::HandlerAdaptor(h))),
            )
        }
        MatterEndpointKind::OnOffLight
        | MatterEndpointKind::OnOffPlug
        | MatterEndpointKind::Fan
        | MatterEndpointKind::GenericSwitch => return Err(not_a_sensor(&cfg.kind)),
    };

    tracing::info!(
        endpoint = id,
        kind = %cfg.kind,
        name = %name,
        source = %source.describe(),
        "Matter: sensor endpoint"
    );

    Ok(EndpointSpec {
        id,
        dynamic: true,
        name,
        device_types: vec![device_type],
        clusters: vec![identify_cluster(rand), (meta, imp)],
    })
}
