// src/radar/mod.rs
//
// Radar sensing (TODO.md Phase 17): a HAL trait + registry, deliberately
// mirroring src/hal/mod.rs's `VideoSource`/`register_source()` pattern
// exactly (same "adding a backend means implementing the trait and
// registering it once, dispatch logic never changes" shape) — not a third
// bespoke factory design. Radar earns its own module, not a fold-in to the
// camera HAL: it's a different sensor category with different output
// shapes (range/azimuth/velocity, not pixels) and a different config
// surface (meters/degrees, not width/height/format).
//
// Why radar gets its own phase: it's optically blind-spot-immune (FMCW/
// mmWave sees through dust, fog, smoke, total darkness where camera and
// LiDAR both degrade) and gives velocity directly via Doppler, no
// frame-differencing or tracker needed — the standard sensing layer for
// quarry/mine haul roads and perimeter security where permanent airborne
// dust defeats optical sensors, directly relevant to this firmware's
// iotmining cloud-relay integration.
//
// Real vendor hardware (TI IWR6843/1843 mmWave UART/TLV, Acconeer A121/
// XM125 I2C/SPI/UART, Xandar Kardian UART/USB, Continental ARS408 CAN,
// Smartmicro UDP/CAN, Navtech TCP) is NOT implemented here — those are
// proprietary/vendor-specific binary wire protocols with no hardware or
// captured traffic available to verify a parser against (the exact
// "don't build infrastructure blind" call already made for Phase 12c/12d
// and the LiDAR HAL). Building a parser for a real protocol without
// anything to test it against is how the WHIP relay's `whipclientsink`
// bug happened — a "verified" claim that was never actually checked
// against a real element. This module ships the full architecture (types,
// trait, registry, analytics, config, cluster fusion) proven against a
// real, tested `mock` backend instead, so every downstream layer is
// genuinely working code today, not a guess. See TODO.md Phase 17a for
// exactly what a new vendor backend needs to implement to plug in.
use crate::config::RadarConfig;
use crate::core::error::EdgeResult;

pub mod analytics;
pub mod mock;

/// One point-tier detection (short/mid-range radar: TI mmWave, Acconeer,
/// Xandar Kardian) — the radar reports raw returns, this firmware clusters
/// and tracks them itself if it needs to.
#[derive(Debug, Clone, PartialEq)]
pub struct RadarPoint {
    pub range_m: f32,
    /// Degrees from boresight (0 = straight ahead), positive = right —
    /// the same left/right sign convention as ONVIF PTZ pan.
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    /// Doppler velocity, m/s. Positive = closing (moving toward the
    /// radar), matching automotive-radar convention — the sign every
    /// vendor's own TLV/CAN docs already use, so a future real backend's
    /// values map straight across with no inversion.
    pub velocity_mps: f32,
    /// Radar cross-section, dBsm — coarse size/reflectivity proxy (a
    /// truck reflects far more than a person). Not a calibrated physical
    /// size measurement, just a relative signal a rule can threshold on.
    pub rcs_dbsm: f32,
}

/// One track-tier detection (long-range/automotive-grade radar that
/// already does internal clustering+tracking: Continental ARS408,
/// Smartmicro, Navtech) — `id` is stable across frames for the same
/// physical object, which point-tier radar's raw returns don't give you.
#[derive(Debug, Clone, PartialEq)]
pub struct RadarTrack {
    pub id: u32,
    pub range_m: f32,
    pub azimuth_deg: f32,
    pub velocity_mps: f32,
    pub rcs_dbsm: f32,
    /// Some radars classify on-device (car/truck/pedestrian) from their
    /// own signal processing — `None` when the unit doesn't offer this.
    pub class_hint: Option<String>,
}

/// Either points or tracks, never both — which one a given radar unit
/// produces is fixed by its own internal signal processing, not something
/// this firmware chooses per frame.
#[derive(Debug, Clone, PartialEq)]
pub enum RadarDetections {
    // Only a real point-tier backend (TI mmWave, Acconeer, Xandar
    // Kardian — none exist yet, see this module's header) constructs
    // this variant; the shipped `mock` backend is track-tier only. Real,
    // exercised logic all the same: `RadarFrame::detections()` and every
    // analytics.rs mode handle it identically to `Tracks` below, tested
    // directly in this module's own test suite.
    #[allow(dead_code)]
    Points(Vec<RadarPoint>),
    Tracks(Vec<RadarTrack>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RadarFrame {
    pub timestamp_ns: u64,
    pub detections: RadarDetections,
}

/// Radar-specific failure modes, mirroring the tamper-style health checks
/// every other sensor HAL in this firmware already reports (14a's LiDAR
/// design: packet-loss/rotation-stall/dirty-window; this firmware's own
/// tamper.rs for cameras).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RadarHealth {
    /// Mutual interference from another nearby radar unit — a real
    /// industrial-site failure mode (multiple radars on one perimeter)
    /// that has no camera equivalent.
    pub interference: bool,
    /// Radome obstruction (mud, ice, dust buildup) — degrades or blinds
    /// the unit without it necessarily going offline.
    pub blocked: bool,
    /// Receiver saturation (a target too close/reflective) — the readings
    /// for this frame are unreliable, not just noisy.
    pub saturated: bool,
}

impl RadarHealth {
    pub fn is_degraded(&self) -> bool {
        self.interference || self.blocked || self.saturated
    }
}

/// A common, tier-agnostic view of one detection — analytics code matches
/// on this instead of the two `RadarDetections` variants directly, so a
/// zone/speed/counting rule doesn't care whether the underlying radar is
/// point-tier or track-tier.
#[derive(Debug, Clone, Copy)]
pub struct Detection<'a> {
    pub range_m: f32,
    pub azimuth_deg: f32,
    pub velocity_mps: f32,
    /// Carried through for a future `RadarZone.min_rcs_dbsm` threshold
    /// (already a config field, not yet consumed by analytics.rs — see
    /// that field's own doc comment for why: no real backend's RCS
    /// calibration exists yet to tune a default against).
    #[allow(dead_code)]
    pub rcs_dbsm: f32,
    pub track_id: Option<u32>,
    pub class_hint: Option<&'a str>,
}

impl RadarFrame {
    /// Normalizes `detections` into the common `Detection` view.
    pub fn detections(&self) -> Vec<Detection<'_>> {
        match &self.detections {
            RadarDetections::Points(points) => points
                .iter()
                .map(|p| Detection {
                    range_m: p.range_m,
                    azimuth_deg: p.azimuth_deg,
                    velocity_mps: p.velocity_mps,
                    rcs_dbsm: p.rcs_dbsm,
                    track_id: None,
                    class_hint: None,
                })
                .collect(),
            RadarDetections::Tracks(tracks) => tracks
                .iter()
                .map(|t| Detection {
                    range_m: t.range_m,
                    azimuth_deg: t.azimuth_deg,
                    velocity_mps: t.velocity_mps,
                    rcs_dbsm: t.rcs_dbsm,
                    track_id: Some(t.id),
                    class_hint: t.class_hint.as_deref(),
                })
                .collect(),
        }
    }
}

/// Range/azimuth (the radar's own native polar output) → a top-down
/// Cartesian plane centered on the radar, meters: x = lateral (positive
/// right, matching `azimuth_deg`'s sign convention), y = forward distance.
/// Zones in `[radar].zones` are defined in this plane — real-world meters,
/// NOT the normalized [0,1] image-frame coordinates `ai.rules`/`motion.zones`
/// use, since radar has no image frame to normalize against.
pub fn polar_to_xy(range_m: f32, azimuth_deg: f32) -> (f32, f32) {
    let azimuth_rad = azimuth_deg.to_radians();
    (range_m * azimuth_rad.sin(), range_m * azimuth_rad.cos())
}

pub trait RadarSource: Send + Sync {
    fn initialize(&mut self) -> EdgeResult<()>;
    fn start(&mut self) -> EdgeResult<()>;
    fn next_frame(&mut self) -> EdgeResult<RadarFrame>;
    fn stop(&mut self) -> EdgeResult<()>;
    /// Best-effort; a backend that can't detect a given failure mode
    /// itself just always reports it clear rather than guessing.
    fn health(&self) -> RadarHealth {
        RadarHealth::default()
    }
}

type RadarCtor = fn(&RadarConfig) -> Box<dyn RadarSource>;

/// Radar backend registry — same shape as `hal::registry()`: adding a real
/// vendor backend means implementing `RadarSource` and calling
/// `register_source()` once in `register_builtins()` below; nothing about
/// `create_radar()`'s dispatch logic changes.
fn registry() -> &'static parking_lot::Mutex<std::collections::HashMap<&'static str, RadarCtor>> {
    static REGISTRY: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<&'static str, RadarCtor>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub(crate) fn register_source(type_name: &'static str, ctor: RadarCtor) {
    registry().lock().insert(type_name, ctor);
}

fn register_builtins() {
    register_source("mock", |cfg| Box::new(mock::MockRadar::new(cfg)));
    // Real vendor backends plug in here, each behind its own module —
    // e.g. register_source("ti_mmwave", |cfg| Box::new(ti_mmwave::TiMmwaveRadar::new(cfg)));
    // — none exist yet; see this module's header comment for why.
}

/// `None` for an unregistered `[radar].type` — unlike the camera HAL
/// (which panics on an unknown Linux backend, since a camera is
/// load-bearing for this firmware's core purpose), radar is an optional
/// additive sensor: a typo'd or not-yet-implemented radar type should
/// disable radar and keep streaming/recording/AI running, never crash-loop
/// the whole device over one optional sensor.
pub fn create_radar(cfg: &RadarConfig) -> Option<Box<dyn RadarSource>> {
    register_builtins();
    registry()
        .lock()
        .get(cfg.r#type.as_str())
        .map(|ctor| ctor(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: u32, class_hint: Option<&str>) -> RadarTrack {
        RadarTrack {
            id,
            range_m: 10.0,
            azimuth_deg: 0.0,
            velocity_mps: 1.0,
            rcs_dbsm: 5.0,
            class_hint: class_hint.map(str::to_string),
        }
    }

    #[test]
    fn detections_normalizes_points_and_tracks_the_same_way() {
        let point_frame = RadarFrame {
            timestamp_ns: 0,
            detections: RadarDetections::Points(vec![RadarPoint {
                range_m: 5.0,
                azimuth_deg: 10.0,
                elevation_deg: 0.0,
                velocity_mps: 2.0,
                rcs_dbsm: 3.0,
            }]),
        };
        let d = point_frame.detections();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].track_id, None);
        assert_eq!(d[0].range_m, 5.0);

        let track_frame = RadarFrame {
            timestamp_ns: 0,
            detections: RadarDetections::Tracks(vec![track(7, Some("truck"))]),
        };
        let d = track_frame.detections();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].track_id, Some(7));
        assert_eq!(d[0].class_hint, Some("truck"));
    }

    #[test]
    fn polar_to_xy_matches_known_cases() {
        // Straight ahead: all range becomes forward (y), no lateral offset.
        let (x, y) = polar_to_xy(10.0, 0.0);
        assert!(x.abs() < 1e-4);
        assert!((y - 10.0).abs() < 1e-4);

        // Due right (90 deg): all range becomes lateral (x), no forward offset.
        let (x, y) = polar_to_xy(10.0, 90.0);
        assert!((x - 10.0).abs() < 1e-3);
        assert!(y.abs() < 1e-3);

        // Due left (-90 deg): negative lateral.
        let (x, _y) = polar_to_xy(10.0, -90.0);
        assert!((x + 10.0).abs() < 1e-3);
    }

    #[test]
    fn radar_health_is_degraded_when_any_flag_set() {
        assert!(!RadarHealth::default().is_degraded());
        assert!(RadarHealth {
            interference: true,
            ..Default::default()
        }
        .is_degraded());
        assert!(RadarHealth {
            blocked: true,
            ..Default::default()
        }
        .is_degraded());
        assert!(RadarHealth {
            saturated: true,
            ..Default::default()
        }
        .is_degraded());
    }

    #[test]
    fn registry_resolves_mock_and_none_for_unknown_type() {
        let mock_cfg = RadarConfig {
            enabled: true,
            r#type: "mock".to_string(),
            ..Default::default()
        };
        assert!(create_radar(&mock_cfg).is_some());

        let unknown_cfg = RadarConfig {
            enabled: true,
            r#type: "not_a_real_backend".to_string(),
            ..Default::default()
        };
        assert!(create_radar(&unknown_cfg).is_none());
    }
}
