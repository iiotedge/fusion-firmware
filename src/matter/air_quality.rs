// src/matter/air_quality.rs
//
// Config-driven Matter Air Quality Sensor (0x002C, Matter 1.2): ONE endpoint
// carrying the AirQuality cluster plus a concentration-measurement cluster per
// configured pollutant (CO2, CO, NO2, ozone, PM1/2.5/10, TVOC, formaldehyde,
// radon) and optionally temperature and humidity — the shape every indoor air
// quality monitor has. Unlike the one-source kinds it is configured with a
// `sources` table:
//
//   [[matter.endpoints]]
//   kind    = "air_quality_sensor"
//   name    = "Living room air"
//   sources = { co2 = "push:co2", pm25 = "sysfs:/sys/bus/iio/devices/iio:device0/in_massconcentration_pm2p5_input" }
//   scales  = { pm25 = 0.001 }
//
// Each value is an ordinary source spec (see src/signals.rs) and `scales`
// brings a source into the Matter unit.
//
// THE AIR QUALITY LEVEL. The AirQuality attribute (Good .. ExtremelyPoor) is
// derived from the readings, using published category breakpoints, because
// nothing else in a plain sensor produces it:
//
//   pollutant        unit    Good   Fair    Moderate  Poor    VeryPoor  (above: ExtremelyPoor)
//   PM2.5            ug/m3   12     35.4    55.4      150.4   250.4     EPA AQI categories
//   PM10             ug/m3   54     154     254       354     424       EPA AQI categories
//   CO               ppm     4.4    9.4     12.4      15.4    30.4      EPA AQI categories
//   NO2              ppb     53     100     360       649     1249      EPA AQI categories
//   ozone            ppb     54     70      85        105     200       EPA AQI categories
//   CO2              ppm     800    1000    1500      2000    5000      common indoor guidance
//
// The level is the WORST of the pollutants that have a reading. PM1, TVOC,
// formaldehyde and radon are reported as measurements but do not move the level
// — there is no broadly accepted general-purpose scale to grade them against,
// and inventing one would be a health claim. The EPA categories are defined on
// time averages; the instantaneous reading used here makes this an indicative
// level, not a regulatory index. A device that computes its own level can supply
// it as `sources.air_quality` (0..=6), which then wins.
//
// HONESTY RULES (as everywhere): a pollutant with no reading, or one outside the
// sensor's physical range, is Matter `null` — never a made-up 0; with no usable
// reading at all the AirQuality level is `Unknown`, which the cluster has a value
// for, rather than "Good".
use core::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rs_matter::dm::clusters::decl::air_quality::{self, AirQualityEnum};
use rs_matter::dm::clusters::decl::globals::{MeasurementMediumEnum, MeasurementUnitEnum};
use rs_matter::dm::clusters::decl::{
    carbon_dioxide_concentration_measurement, carbon_monoxide_concentration_measurement,
    formaldehyde_concentration_measurement, nitrogen_dioxide_concentration_measurement,
    ozone_concentration_measurement, pm_10_concentration_measurement,
    pm_1_concentration_measurement, pm_25_concentration_measurement,
    radon_concentration_measurement, total_volatile_organic_compounds_concentration_measurement,
};
use rs_matter::dm::{Async, Cluster, Dataver, DeviceType, EndptId, HandlerContext, ReadContext};
use rs_matter::error::Error;
use rs_matter::tlv::Nullable;
use rs_matter::with;

use crate::config::{effective_endpoint_name, effective_poll_ms, MatterEndpointConfig, MatterEndpointKind};
use crate::matter::registry::{identify_cluster, ClusterImpl, EndpointSpec};
use crate::matter::sensors::{
    humidity_cluster, humidity_range, resolve_spec, temperature_cluster, temperature_range, watch_attr,
    Common, ATTR_PRIMARY,
};
use crate::signals::{SignalBus, Source};

use tracing::info;

/// One measurable pollutant on an air quality endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Pollutant {
    Co,
    Co2,
    No2,
    O3,
    Pm1,
    Pm25,
    Pm10,
    Tvoc,
    Formaldehyde,
    Radon,
}

impl Pollutant {
    /// `sources` keys, in the order they are documented.
    pub(crate) const ALL: [(&'static str, Pollutant); 10] = [
        ("co2", Self::Co2),
        ("co", Self::Co),
        ("no2", Self::No2),
        ("o3", Self::O3),
        ("pm1", Self::Pm1),
        ("pm25", Self::Pm25),
        ("pm10", Self::Pm10),
        ("tvoc", Self::Tvoc),
        ("formaldehyde", Self::Formaldehyde),
        ("radon", Self::Radon),
    ];

    pub(crate) fn parse(key: &str) -> Option<Self> {
        Self::ALL.iter().find(|(k, _)| *k == key).map(|(_, p)| *p)
    }

    /// The Matter unit a source must be in (use `scales` to convert).
    fn unit(self) -> MeasurementUnitEnum {
        match self {
            Self::Co | Self::Co2 => MeasurementUnitEnum::PPM,
            Self::No2 | Self::O3 | Self::Tvoc | Self::Formaldehyde => MeasurementUnitEnum::PPB,
            Self::Pm1 | Self::Pm25 | Self::Pm10 => MeasurementUnitEnum::UGM3,
            Self::Radon => MeasurementUnitEnum::BQM3,
        }
    }

    /// A sensor's plausible physical range in that unit; a reading outside it is
    /// not trusted (it reads as `null`).
    fn range(self) -> (f64, f64) {
        match self {
            Self::Co2 => (0.0, 10_000.0),
            Self::Co => (0.0, 1_000.0),
            Self::No2 => (0.0, 2_000.0),
            Self::O3 => (0.0, 1_000.0),
            Self::Pm1 | Self::Pm25 | Self::Pm10 => (0.0, 1_000.0),
            Self::Tvoc => (0.0, 60_000.0),
            Self::Formaldehyde => (0.0, 5_000.0),
            Self::Radon => (0.0, 10_000.0),
        }
    }

    /// Upper bound of Good, Fair, Moderate, Poor and VeryPoor (see the table in
    /// the module header); above the last is ExtremelyPoor. `None` = this
    /// pollutant is reported but not graded.
    fn breakpoints(self) -> Option<[f64; 5]> {
        match self {
            Self::Pm25 => Some([12.0, 35.4, 55.4, 150.4, 250.4]),
            Self::Pm10 => Some([54.0, 154.0, 254.0, 354.0, 424.0]),
            Self::Co => Some([4.4, 9.4, 12.4, 15.4, 30.4]),
            Self::No2 => Some([53.0, 100.0, 360.0, 649.0, 1249.0]),
            Self::O3 => Some([54.0, 70.0, 85.0, 105.0, 200.0]),
            Self::Co2 => Some([800.0, 1000.0, 1500.0, 2000.0, 5000.0]),
            Self::Pm1 | Self::Tvoc | Self::Formaldehyde | Self::Radon => None,
        }
    }

    /// `AirQualityEnum` value for a reading: 1 = Good .. 6 = ExtremelyPoor, or
    /// `None` when this pollutant is not graded.
    fn level(self, value: f64) -> Option<u8> {
        let bounds = self.breakpoints()?;
        Some(bounds.iter().position(|b| value <= *b).map_or(6, |i| i as u8 + 1))
    }
}

/// Is `key` something an air quality endpoint's `sources` table may contain?
pub(crate) fn is_source_key(key: &str) -> bool {
    Pollutant::parse(key).is_some() || matches!(key, "temperature" | "humidity" | "air_quality")
}

/// The valid `sources` keys, for error messages.
pub(crate) fn source_keys() -> String {
    let mut keys: Vec<&str> = Pollutant::ALL.iter().map(|(k, _)| *k).collect();
    keys.extend(["temperature", "humidity", "air_quality"]);
    keys.join(", ")
}

/// An air quality endpoint that measures nothing and brings no level of its own
/// would report Unknown forever, so it needs at least one pollutant or a level.
pub(crate) fn has_air_quality_input<'a>(mut keys: impl Iterator<Item = &'a String>) -> bool {
    keys.any(|k| Pollutant::parse(k).is_some() || k == "air_quality")
}

/// The worst graded level (1 = Good .. 6 = ExtremelyPoor) over the readings, or 0
/// (Unknown) when none of them is graded.
fn derive_level(readings: impl IntoIterator<Item = (Pollutant, f64)>) -> u8 {
    readings
        .into_iter()
        .filter_map(|(pollutant, value)| pollutant.level(value))
        .max()
        .unwrap_or(0)
}

fn air_quality_enum(level: u8) -> AirQualityEnum {
    match level {
        1 => AirQualityEnum::Good,
        2 => AirQualityEnum::Fair,
        3 => AirQualityEnum::Moderate,
        4 => AirQualityEnum::Poor,
        5 => AirQualityEnum::VeryPoor,
        6 => AirQualityEnum::ExtremelyPoor,
        _ => AirQualityEnum::Unknown,
    }
}

/// One pollutant reading as the AirQuality level sees it: the same scale and
/// range rule the concentration cluster applies, so the two never disagree.
struct Input {
    pollutant: Pollutant,
    source: Arc<dyn Source>,
    scale: f64,
}

impl Input {
    fn value(&self) -> Option<f64> {
        let value = self.source.read()?.value.as_f64() * self.scale;
        let (min, max) = self.pollutant.range();
        (value.is_finite() && value >= min && value <= max).then_some(value)
    }
}

pub(crate) struct AirQualityHandler {
    endpoint: EndptId,
    dataver: Dataver,
    poll: Duration,
    /// A level the device already computes (0..=6): wins over the derived one.
    direct: Option<Arc<dyn Source>>,
    inputs: Vec<Input>,
}

impl AirQualityHandler {
    /// 0 = Unknown .. 6 = ExtremelyPoor.
    fn level(&self) -> u8 {
        if let Some(source) = &self.direct {
            // The device said it computes its own: an invalid value is Unknown,
            // never silently replaced by ours.
            return match source.read().map(|r| r.value.as_f64()) {
                Some(v) if v.is_finite() && (0.0..=6.0).contains(&v) => v.round() as u8,
                _ => 0,
            };
        }
        derive_level(
            self.inputs
                .iter()
                .filter_map(|i| i.value().map(|v| (i.pollutant, v))),
        )
    }
}

impl air_quality::ClusterHandler for AirQualityHandler {
    // All four optional features: every level from Good to ExtremelyPoor.
    const CLUSTER: Cluster<'static> = air_quality::FULL_CLUSTER
        .with_features(
            air_quality::Feature::FAIR.bits()
                | air_quality::Feature::MODERATE.bits()
                | air_quality::Feature::VERY_POOR.bits()
                | air_quality::Feature::EXTREMELY_POOR.bits(),
        )
        .with_attrs(with!(required))
        .with_cmds(with!());

    fn dataver(&self) -> u32 {
        self.dataver.get()
    }

    fn dataver_changed(&self) {
        self.dataver.changed();
    }

    fn air_quality(&self, _ctx: impl ReadContext) -> Result<AirQualityEnum, Error> {
        Ok(air_quality_enum(self.level()))
    }

    fn run(&self, ctx: impl HandlerContext) -> impl Future<Output = Result<(), Error>> {
        watch_attr(ctx, self.endpoint, self.poll, 0x005B, ATTR_PRIMARY, || self.level())
    }
}

/// Generates one concentration-measurement handler per pollutant from a table,
/// plus `make_concentration`, which builds the right one for a `Pollutant`.
macro_rules! concentration_clusters {
    ($( $pollutant:ident: $handler:ident, $module:ident, $variant:ident, $cluster_id:expr; )+) => {
        $(
            pub(crate) struct $handler {
                common: Common,
                min: f32,
                max: f32,
            }

            impl $handler {
                /// Matter-visible `MeasuredValue`: `None` = null.
                fn sample(&self) -> Option<f32> {
                    self.common.numeric().map(|v| v as f32)
                }
            }

            impl $module::ClusterHandler for $handler {
                // Numeric measurement only: no level indication, peak or average.
                const CLUSTER: Cluster<'static> = $module::FULL_CLUSTER
                    .with_features($module::Feature::NUMERIC_MEASUREMENT.bits())
                    .with_attrs(with!(
                        required;
                        $module::AttributeId::MeasuredValue
                            | $module::AttributeId::MinMeasuredValue
                            | $module::AttributeId::MaxMeasuredValue
                            | $module::AttributeId::MeasurementUnit
                    ))
                    .with_cmds(with!());

                fn dataver(&self) -> u32 {
                    self.common.dataver.get()
                }

                fn dataver_changed(&self) {
                    self.common.dataver.changed();
                }

                fn measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<f32>, Error> {
                    Ok(match self.sample() {
                        Some(v) => Nullable::some(v),
                        None => Nullable::none(),
                    })
                }

                fn min_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<f32>, Error> {
                    Ok(Nullable::some(self.min))
                }

                fn max_measured_value(&self, _ctx: impl ReadContext) -> Result<Nullable<f32>, Error> {
                    Ok(Nullable::some(self.max))
                }

                fn measurement_unit(&self, _ctx: impl ReadContext) -> Result<MeasurementUnitEnum, Error> {
                    Ok(Pollutant::$pollutant.unit())
                }

                fn measurement_medium(&self, _ctx: impl ReadContext) -> Result<MeasurementMediumEnum, Error> {
                    Ok(MeasurementMediumEnum::Air)
                }

                fn run(&self, ctx: impl HandlerContext) -> impl Future<Output = Result<(), Error>> {
                    self.common.watch(ctx, $cluster_id, || self.sample())
                }
            }
        )+

        /// The concentration cluster for `pollutant`, bound to `common`.
        fn make_concentration(pollutant: Pollutant, common: Common) -> (Cluster<'static>, ClusterImpl) {
            let (min, max) = pollutant.range();
            let (min, max) = (min as f32, max as f32);
            match pollutant {
                $(
                    Pollutant::$pollutant => {
                        let h = $handler { common, min, max };
                        (
                            <$handler as $module::ClusterHandler>::CLUSTER,
                            ClusterImpl::$variant(Async($module::HandlerAdaptor(h))),
                        )
                    }
                )+
            }
        }
    };
}

concentration_clusters! {
    Co:           CoHandler,           carbon_monoxide_concentration_measurement,                    ConcCo,           0x040C;
    Co2:          Co2Handler,          carbon_dioxide_concentration_measurement,                    ConcCo2,          0x040D;
    No2:          No2Handler,          nitrogen_dioxide_concentration_measurement,                  ConcNo2,          0x0413;
    O3:           O3Handler,           ozone_concentration_measurement,                             ConcO3,           0x0415;
    Pm1:          Pm1Handler,          pm_1_concentration_measurement,                              ConcPm1,          0x042C;
    Pm25:         Pm25Handler,         pm_25_concentration_measurement,                             ConcPm25,         0x042A;
    Pm10:         Pm10Handler,         pm_10_concentration_measurement,                             ConcPm10,         0x042D;
    Tvoc:         TvocHandler,         total_volatile_organic_compounds_concentration_measurement,  ConcTvoc,         0x042E;
    Formaldehyde: FormaldehydeHandler, formaldehyde_concentration_measurement,                      ConcFormaldehyde, 0x042B;
    Radon:        RadonHandler,        radon_concentration_measurement,                             ConcRadon,        0x042F;
}

fn common(id: EndptId, source: Arc<dyn Source>, poll: Duration, scale: f64, range: (f64, f64), rand: &mut impl rand_core::Rng) -> Common {
    Common {
        endpoint: id,
        source,
        poll,
        dataver: Dataver::new_rand(rand),
        scale,
        offset: 0.0,
        nat_min: range.0,
        nat_max: range.1,
        invert: false,
        hold: None,
    }
}

/// Build one `air_quality_sensor` entry. Fails (so the caller can skip just this
/// endpoint) on an unusable source.
pub(crate) fn build_endpoint<R: rand_core::Rng>(
    cfg: &MatterEndpointConfig,
    index: usize,
    id: EndptId,
    bus: &SignalBus,
    rand: &mut R,
) -> Result<EndpointSpec, String> {
    let name = effective_endpoint_name(cfg, index);
    let poll = Duration::from_millis(effective_poll_ms(MatterEndpointKind::AirQuality, cfg));

    let mut measurements: Vec<(Cluster<'static>, ClusterImpl)> = Vec::new();
    let mut inputs = Vec::new();
    let mut direct = None;
    let mut described = Vec::new();

    // BTreeMap: a deterministic, alphabetical cluster order.
    for (key, spec) in &cfg.sources {
        let source = resolve_spec(spec, cfg.allow_mock, bus).map_err(|e| format!("sources.{key}: {e}"))?;
        let scale = cfg.scales.get(key).copied().unwrap_or(1.0);
        described.push(format!("{key}={}", source.describe()));
        match key.as_str() {
            "air_quality" => direct = Some(source),
            "temperature" => {
                measurements.push(temperature_cluster(common(id, source, poll, scale, temperature_range(), rand)));
            }
            "humidity" => {
                measurements.push(humidity_cluster(common(id, source, poll, scale, humidity_range(), rand)));
            }
            other => {
                let pollutant = Pollutant::parse(other)
                    .ok_or_else(|| format!("unknown sources key '{other}' (available: {})", source_keys()))?;
                measurements.push(make_concentration(
                    pollutant,
                    common(id, source.clone(), poll, scale, pollutant.range(), rand),
                ));
                inputs.push(Input { pollutant, source, scale });
            }
        }
    }

    let handler = AirQualityHandler {
        endpoint: id,
        dataver: Dataver::new_rand(rand),
        poll,
        direct,
        inputs,
    };

    info!(
        endpoint = id,
        name = %name,
        sources = %described.join(", "),
        "Matter: air quality endpoint"
    );

    let (dtype, drev) = MatterEndpointKind::AirQuality.device_type();
    let mut clusters = vec![
        identify_cluster(rand),
        (
            <AirQualityHandler as air_quality::ClusterHandler>::CLUSTER,
            ClusterImpl::AirQuality(Async(air_quality::HandlerAdaptor(handler))),
        ),
    ];
    clusters.extend(measurements);
    Ok(EndpointSpec {
        id,
        dynamic: true,
        name,
        device_types: vec![DeviceType { dtype, drev }],
        clusters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{FnSource, Provenance, Value};

    fn level_of(readings: &[(Pollutant, f64)]) -> u8 {
        derive_level(readings.iter().copied())
    }

    #[test]
    fn every_source_key_parses_and_has_a_unit() {
        for (key, pollutant) in Pollutant::ALL {
            assert_eq!(Pollutant::parse(key), Some(pollutant), "{key}");
            assert!(is_source_key(key));
            let (lo, hi) = pollutant.range();
            assert!(lo < hi, "{key}");
        }
        for extra in ["temperature", "humidity", "air_quality"] {
            assert!(is_source_key(extra));
        }
        assert!(!is_source_key("pm2.5"));
        assert!(!is_source_key(""));
        assert_eq!(Pollutant::Co2.unit(), MeasurementUnitEnum::PPM);
        assert_eq!(Pollutant::Pm25.unit(), MeasurementUnitEnum::UGM3);
        assert_eq!(Pollutant::Tvoc.unit(), MeasurementUnitEnum::PPB);
        assert_eq!(Pollutant::Radon.unit(), MeasurementUnitEnum::BQM3);
    }

    #[test]
    fn breakpoints_are_strictly_increasing() {
        for (key, p) in Pollutant::ALL {
            if let Some(b) = p.breakpoints() {
                assert!(b.windows(2).all(|w| w[0] < w[1]), "{key}: {b:?}");
            }
        }
    }

    #[test]
    fn pm25_levels_follow_the_epa_categories() {
        use Pollutant::Pm25;
        assert_eq!(level_of(&[(Pm25, 0.0)]), 1, "Good");
        assert_eq!(level_of(&[(Pm25, 12.0)]), 1, "12 is the top of Good");
        assert_eq!(level_of(&[(Pm25, 12.1)]), 2, "Fair");
        assert_eq!(level_of(&[(Pm25, 40.0)]), 3, "Moderate");
        assert_eq!(level_of(&[(Pm25, 100.0)]), 4, "Poor");
        assert_eq!(level_of(&[(Pm25, 200.0)]), 5, "VeryPoor");
        assert_eq!(level_of(&[(Pm25, 300.0)]), 6, "ExtremelyPoor");
    }

    #[test]
    fn co2_levels_follow_indoor_guidance() {
        use Pollutant::Co2;
        assert_eq!(level_of(&[(Co2, 450.0)]), 1);
        assert_eq!(level_of(&[(Co2, 900.0)]), 2);
        assert_eq!(level_of(&[(Co2, 1200.0)]), 3);
        assert_eq!(level_of(&[(Co2, 1800.0)]), 4);
        assert_eq!(level_of(&[(Co2, 2600.0)]), 5);
        assert_eq!(level_of(&[(Co2, 6000.0)]), 6);
    }

    #[test]
    fn the_level_is_the_worst_graded_pollutant() {
        use Pollutant::*;
        assert_eq!(level_of(&[(Co2, 450.0), (Pm25, 40.0)]), 3, "PM2.5 drags it down");
        assert_eq!(level_of(&[(Co2, 2600.0), (Pm25, 5.0)]), 5, "so does CO2");
    }

    #[test]
    fn ungraded_pollutants_are_reported_but_do_not_move_the_level() {
        use Pollutant::*;
        assert_eq!(level_of(&[(Tvoc, 50_000.0), (Radon, 9_000.0), (Pm1, 900.0), (Formaldehyde, 4_000.0)]), 0);
        assert_eq!(level_of(&[(Co2, 450.0), (Tvoc, 50_000.0)]), 1, "only the graded one counts");
    }

    #[test]
    fn no_graded_reading_is_unknown_never_good() {
        assert_eq!(level_of(&[]), 0);
        assert_eq!(air_quality_enum(0), AirQualityEnum::Unknown);
        assert_eq!(air_quality_enum(1), AirQualityEnum::Good);
        assert_eq!(air_quality_enum(6), AirQualityEnum::ExtremelyPoor);
        assert_eq!(air_quality_enum(9), AirQualityEnum::Unknown);
    }

    fn handler(inputs: Vec<(Pollutant, Option<f64>)>, direct: Option<Option<f64>>) -> AirQualityHandler {
        let src = |v: Option<f64>| -> Arc<dyn Source> {
            Arc::new(FnSource::new("t", Provenance::Real, move || v.map(Value::Num)))
        };
        AirQualityHandler {
            endpoint: 1,
            dataver: Dataver::new(1),
            poll: Duration::from_millis(100),
            direct: direct.map(src),
            inputs: inputs
                .into_iter()
                .map(|(pollutant, v)| Input { pollutant, source: src(v), scale: 1.0 })
                .collect(),
        }
    }

    #[test]
    fn out_of_range_and_missing_readings_do_not_contribute() {
        use Pollutant::*;
        // PM2.5 of 5000 ug/m3 is outside any real sensor's range: not trusted.
        let h = handler(vec![(Pm25, Some(5000.0)), (Co2, Some(450.0))], None);
        assert_eq!(h.level(), 1, "only CO2 counts");
        // Nothing usable at all.
        assert_eq!(handler(vec![(Pm25, None), (Co2, None)], None).level(), 0);
    }

    #[test]
    fn a_scale_brings_a_source_into_the_matter_unit() {
        let src: Arc<dyn Source> = Arc::new(FnSource::new("t", Provenance::Real, || Some(Value::Num(40_000.0))));
        // 40000 in ng/m3 * 0.001 = 40 ug/m3 -> Moderate
        let input = Input { pollutant: Pollutant::Pm25, source: src, scale: 0.001 };
        assert_eq!(input.value(), Some(40.0));
        assert_eq!(Pollutant::Pm25.level(40.0), Some(3));
    }

    #[test]
    fn a_device_computed_level_wins_and_a_bad_one_is_unknown_not_replaced() {
        use Pollutant::*;
        let h = handler(vec![(Pm25, Some(300.0))], Some(Some(2.0)));
        assert_eq!(h.level(), 2, "the device's own level wins over the derived 6");
        assert_eq!(handler(vec![(Pm25, Some(300.0))], Some(Some(9.0))).level(), 0, "invalid -> Unknown");
        assert_eq!(handler(vec![(Pm25, Some(300.0))], Some(None)).level(), 0, "no reading -> Unknown");
    }

    #[test]
    fn the_cluster_metadata_matches_what_is_implemented() {
        let c = <AirQualityHandler as air_quality::ClusterHandler>::CLUSTER;
        for f in [
            air_quality::Feature::FAIR,
            air_quality::Feature::MODERATE,
            air_quality::Feature::VERY_POOR,
            air_quality::Feature::EXTREMELY_POOR,
        ] {
            assert_ne!(c.feature_map & f.bits(), 0, "{f:?}");
        }
        let co2 = <Co2Handler as carbon_dioxide_concentration_measurement::ClusterHandler>::CLUSTER;
        assert_ne!(co2.feature_map & carbon_dioxide_concentration_measurement::Feature::NUMERIC_MEASUREMENT.bits(), 0);
        for a in [
            carbon_dioxide_concentration_measurement::AttributeId::MeasuredValue,
            carbon_dioxide_concentration_measurement::AttributeId::MinMeasuredValue,
            carbon_dioxide_concentration_measurement::AttributeId::MaxMeasuredValue,
            carbon_dioxide_concentration_measurement::AttributeId::MeasurementUnit,
            carbon_dioxide_concentration_measurement::AttributeId::MeasurementMedium,
        ] {
            assert!(co2.attribute(a as _).is_some(), "{a:?}");
        }
        assert!(co2
            .attribute(carbon_dioxide_concentration_measurement::AttributeId::PeakMeasuredValue as _)
            .is_none());
    }

    #[test]
    fn has_air_quality_input_needs_a_pollutant_or_a_direct_level() {
        let keys = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(has_air_quality_input(keys(&["co2"]).iter()));
        assert!(has_air_quality_input(keys(&["air_quality", "temperature"]).iter()));
        assert!(!has_air_quality_input(keys(&["temperature", "humidity"]).iter()));
        assert!(!has_air_quality_input(keys(&[]).iter()));
    }
}
