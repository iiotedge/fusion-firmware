// src/radar/analytics.rs
//
// Radar analytics (Phase 17b): zone presence/intrusion, line-crossing
// (which doubles as directional counting — a peer webhook/telemetry
// consumer tallies direction-separated crossings, no counter state kept
// in-firmware, the exact same design choice Phase 16's ai.rules line_cross
// mode already made), and loitering. Deliberately the same three-mode
// vocabulary as src/ai/rules.rs ("presence" | "line_cross" | "loiter") —
// one set of concepts across every analytics engine in this firmware, not
// a second one reinvented for radar. Direct speed measurement needs no
// code here at all: Doppler velocity is already a field on every
// detection (RadarPoint::velocity_mps / RadarTrack::velocity_mps) — a rule
// consumer thresholds RadarEvent::velocity_mps directly, no estimation or
// tracker required, unlike camera-only speed which would need one.
//
// Zone geometry lives in the top-down x,y plane meters (radar::polar_to_xy
// converts each detection into it before testing) — NOT normalized [0,1]
// image coordinates, since radar has no image frame. Point-in-polygon and
// side-of-line are the same ray-casting/cross-product algorithms
// src/ai/rules.rs uses, reimplemented here rather than exported from that
// module: this operates on real-world meters, that module on normalized
// frame fractions — sharing the formula without sharing the misleading
// implication that the units are interchangeable.
//
// NOT implemented: micro-Doppler classification (vibration/gait-signature
// human-vs-vehicle-vs-vegetation discrimination). That needs raw ADC/
// spectrogram access most point/track-tier radar output doesn't expose —
// the processed detection list this firmware consumes (RadarPoint/
// RadarTrack) has already thrown that information away by the time it
// reaches software. Flagged honestly as a gap here rather than shipped as
// a classifier that wouldn't actually classify anything real — see
// TODO.md Phase 17b.
use crate::config::RadarZone;
use crate::radar::{polar_to_xy, RadarFrame};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct RadarEvent {
    pub zone_name: String,
    pub mode: String,
    pub track_id: Option<u32>,
    pub class_hint: Option<String>,
    pub range_m: f32,
    pub azimuth_deg: f32,
    pub velocity_mps: f32,
}

/// Keys a detection's tracked state: a track-tier radar's own stable `id`
/// when available, otherwise a coarse position bucket for point-tier
/// radar — same "no real tracker, bucket by something stable enough"
/// approach src/ai/rules.rs's loiter/line-cross modes already use for
/// camera detections with no ID of their own.
type TrackKey = (Option<u32>, i32);

const BUCKET_METERS: f32 = 0.5;
const STALE_AFTER: Duration = Duration::from_secs(5);
/// A detection at near-zero velocity, seen in the same position bucket
/// this many consecutive frames, is almost certainly a static reflector
/// (fence post, parked equipment, foliage) rather than a genuine
/// intrusion — suppressed from zone evaluation entirely. This is radar's
/// equivalent of Phase 14b's dust/rain filtering.
const CLUTTER_VELOCITY_THRESHOLD_MPS: f32 = 0.2;
const CLUTTER_CONSECUTIVE_FRAMES: u32 = 20;

struct TrackState {
    last_seen: Instant,
    side: Option<bool>,
    entered_at: Option<Instant>,
    fired: bool,
}

struct CompiledZone {
    cfg: RadarZone,
    points_xy: Vec<(f32, f32)>,
    tracks: HashMap<TrackKey, TrackState>,
}

struct ClutterEntry {
    consecutive_frames: u32,
}

pub struct RadarAnalyzer {
    zones: Vec<CompiledZone>,
    clutter: HashMap<(i32, i32), ClutterEntry>,
}

impl RadarAnalyzer {
    pub fn new(zones: &[RadarZone]) -> Self {
        let compiled = zones
            .iter()
            .map(|z| CompiledZone {
                cfg: z.clone(),
                points_xy: z.points.iter().map(|[x, y]| (*x, *y)).collect(),
                tracks: HashMap::new(),
            })
            .collect();
        Self {
            zones: compiled,
            clutter: HashMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    pub fn evaluate(&mut self, frame: &RadarFrame) -> Vec<RadarEvent> {
        let now = Instant::now();
        let detections = frame.detections();
        let live: Vec<_> = detections
            .into_iter()
            .filter(|det| !self.is_clutter(det.range_m, det.azimuth_deg, det.velocity_mps))
            .collect();

        let mut events = Vec::new();
        for zone in &mut self.zones {
            prune_stale(&mut zone.tracks, now);
            if !zone.cfg.enabled {
                continue;
            }
            for det in &live {
                let (x, y) = polar_to_xy(det.range_m, det.azimuth_deg);
                match zone.cfg.mode.as_str() {
                    "presence" => {
                        if point_in_polygon(x, y, &zone.points_xy) {
                            events.push(new_event(zone, det));
                        }
                    }
                    "line_cross" => {
                        if zone.points_xy.len() == 2 {
                            let key = line_cross_key(
                                det.track_id,
                                zone.points_xy[0],
                                zone.points_xy[1],
                                (x, y),
                            );
                            if let Some(event) = evaluate_line_cross(zone, key, (x, y), now, det) {
                                events.push(event);
                            }
                        }
                    }
                    "loiter" => {
                        // Keyed by class_hint/"any" alone, not position —
                        // same reasoning as ai/rules.rs's loiter mode: a
                        // single object's dwell timer must not fragment as
                        // it drifts within the zone. See that module's
                        // comment for the full tradeoff (multiple same-
                        // class objects in one zone can confuse this).
                        let loiter_key: TrackKey =
                            (None, det.class_hint.map(|c| c.len() as i32).unwrap_or(-1));
                        if let Some(event) =
                            evaluate_loiter(zone, loiter_key, (x, y), now, det, zone.cfg.dwell_s)
                        {
                            events.push(event);
                        }
                    }
                    _ => {}
                }
            }
        }
        events
    }

    /// `true` if this detection matches the clutter signature (near-zero
    /// velocity, same position bucket, seen many consecutive frames) —
    /// updates the clutter map either way so a genuinely moving object
    /// clears its bucket's count immediately.
    fn is_clutter(&mut self, range_m: f32, azimuth_deg: f32, velocity_mps: f32) -> bool {
        let (x, y) = polar_to_xy(range_m, azimuth_deg);
        let bucket = (
            (x / BUCKET_METERS).round() as i32,
            (y / BUCKET_METERS).round() as i32,
        );
        if velocity_mps.abs() >= CLUTTER_VELOCITY_THRESHOLD_MPS {
            self.clutter.remove(&bucket);
            return false;
        }
        let entry = self.clutter.entry(bucket).or_insert(ClutterEntry {
            consecutive_frames: 0,
        });
        entry.consecutive_frames += 1;
        entry.consecutive_frames > CLUTTER_CONSECUTIVE_FRAMES
    }
}

/// Line-crossing state key: a track-tier detection already has a stable
/// `id` (position-independent — using it directly is exactly right, since
/// the id doesn't change as the object moves). A point-tier detection (no
/// id) is keyed by its position PROJECTED ALONG the line's own direction,
/// NOT raw (x,y) — the same fix Phase 16's `ai/rules.rs` line-crossing
/// needed: an object moving perpendicular to the line (the normal
/// crossing motion) must stay in the same bucket throughout the crossing,
/// which raw 2D bucketing does not guarantee (it lands in a different
/// bucket mid-crossing, losing the "which side was it on" state).
fn line_cross_key(track_id: Option<u32>, a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> TrackKey {
    match track_id {
        Some(id) => (Some(id), 0),
        None => (None, line_bucket(a, b, p)),
    }
}

fn line_bucket(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> i32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len_sq = dx * dx + dy * dy;
    if len_sq < f32::EPSILON {
        return 0;
    }
    let t = ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len_sq;
    let line_len_m = len_sq.sqrt();
    ((t * line_len_m) / BUCKET_METERS) as i32
}

fn prune_stale(tracks: &mut HashMap<TrackKey, TrackState>, now: Instant) {
    tracks.retain(|_, t| now.duration_since(t.last_seen) < STALE_AFTER);
}

fn new_event(zone: &CompiledZone, det: &crate::radar::Detection) -> RadarEvent {
    RadarEvent {
        zone_name: zone.cfg.name.clone(),
        mode: zone.cfg.mode.clone(),
        track_id: det.track_id,
        class_hint: det.class_hint.map(str::to_string),
        range_m: det.range_m,
        azimuth_deg: det.azimuth_deg,
        velocity_mps: det.velocity_mps,
    }
}

fn evaluate_line_cross(
    zone: &mut CompiledZone,
    key: TrackKey,
    point: (f32, f32),
    now: Instant,
    det: &crate::radar::Detection,
) -> Option<RadarEvent> {
    let (a, b) = (zone.points_xy[0], zone.points_xy[1]);
    let side_now = side_of_line(a, b, point);
    let track = zone.tracks.entry(key).or_insert(TrackState {
        last_seen: now,
        side: None,
        entered_at: None,
        fired: false,
    });
    let previous_side = track.side;
    track.side = Some(side_now);
    track.last_seen = now;

    let Some(previous_side) = previous_side else {
        return None; // first sighting — no transition to detect yet
    };
    if previous_side == side_now {
        return None; // no crossing this frame
    }
    // side_of_line's cross product is positive (true) to the LEFT of the
    // directed line a->b (standard 2D perp-dot convention — see that
    // function's own doc comment): "a_to_b" is defined as crossing FROM
    // the right of that directed line TO the left of it (false -> true),
    // "b_to_a" the reverse. Which physical direction that means on site
    // depends on how `points` was ordered when the zone was configured —
    // same "direction is relative to your own two points, pick a_to_b/
    // b_to_a by testing once" reality ai.rules' line_cross mode already
    // has.
    let direction_matches = match zone.cfg.direction.as_str() {
        "a_to_b" => !previous_side && side_now,
        "b_to_a" => previous_side && !side_now,
        _ => true, // "either" (or unset — validated at config load)
    };
    direction_matches.then(|| new_event(zone, det))
}

fn evaluate_loiter(
    zone: &mut CompiledZone,
    key: TrackKey,
    point: (f32, f32),
    now: Instant,
    det: &crate::radar::Detection,
    dwell_s: u64,
) -> Option<RadarEvent> {
    let inside = point_in_polygon(point.0, point.1, &zone.points_xy);
    if !inside {
        zone.tracks.remove(&key);
        return None;
    }
    let track = zone.tracks.entry(key).or_insert(TrackState {
        last_seen: now,
        side: None,
        entered_at: Some(now),
        fired: false,
    });
    track.last_seen = now;
    let entered_at = *track.entered_at.get_or_insert(now);
    if track.fired {
        return None;
    }
    if now.duration_since(entered_at) >= Duration::from_secs(dwell_s) {
        track.fired = true;
        return Some(new_event(zone, det));
    }
    None
}

fn point_in_polygon(px: f32, py: f32, polygon: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if ((yi > py) != (yj > py)) && (px < (xj - xi) * (py - yi) / (yj - yi) + xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn side_of_line(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> bool {
    let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    cross >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::radar::{RadarDetections, RadarTrack};

    fn zone(name: &str, mode: &str, points: Vec<[f32; 2]>) -> RadarZone {
        RadarZone {
            name: name.to_string(),
            enabled: true,
            mode: mode.to_string(),
            points,
            direction: String::new(),
            dwell_s: 0,
            min_rcs_dbsm: None,
        }
    }

    fn track_frame(range_m: f32, azimuth_deg: f32, velocity_mps: f32, id: u32) -> RadarFrame {
        RadarFrame {
            timestamp_ns: 0,
            detections: RadarDetections::Tracks(vec![RadarTrack {
                id,
                range_m,
                azimuth_deg,
                velocity_mps,
                rcs_dbsm: 8.0,
                class_hint: Some("vehicle".to_string()),
            }]),
        }
    }

    #[test]
    fn presence_fires_when_a_track_enters_the_polygon() {
        let mut analyzer = RadarAnalyzer::new(&[zone(
            "hazard",
            "presence",
            vec![[-5.0, 5.0], [5.0, 5.0], [5.0, 20.0], [-5.0, 20.0]],
        )]);
        // Far outside range/azimuth maps to (0, 50) — outside the zone.
        assert!(analyzer
            .evaluate(&track_frame(50.0, 0.0, 5.0, 1))
            .is_empty());
        // Straight ahead at 10m maps to (0, 10) — inside the zone.
        let events = analyzer.evaluate(&track_frame(10.0, 0.0, 5.0, 1));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].zone_name, "hazard");
        assert_eq!(events[0].velocity_mps, 5.0);
    }

    #[test]
    fn line_cross_fires_only_on_direction_match() {
        let mut a_to_b = RadarAnalyzer::new(&[{
            let mut z = zone("gate", "line_cross", vec![[-10.0, 20.0], [10.0, 20.0]]);
            z.direction = "a_to_b".to_string();
            z
        }]);
        // Approaching from y < 20 (side "true" for this line orientation)
        // then crossing to y > 20.
        assert!(a_to_b.evaluate(&track_frame(10.0, 0.0, 1.0, 1)).is_empty()); // first sighting, no transition yet
        let far_side_frame = RadarFrame {
            timestamp_ns: 0,
            detections: RadarDetections::Tracks(vec![RadarTrack {
                id: 1,
                range_m: 25.0,
                azimuth_deg: 0.0,
                velocity_mps: 1.0,
                rcs_dbsm: 8.0,
                class_hint: None,
            }]),
        };
        let events = a_to_b.evaluate(&far_side_frame);
        assert_eq!(
            events.len(),
            1,
            "should fire once on the matching-direction crossing"
        );
    }

    #[test]
    fn line_cross_wrong_direction_never_fires() {
        // Same physical crossing as the a_to_b test above, but this zone
        // only wants b_to_a — the reverse direction, so it must never fire.
        let mut b_to_a = RadarAnalyzer::new(&[{
            let mut z = zone("gate", "line_cross", vec![[-10.0, 20.0], [10.0, 20.0]]);
            z.direction = "b_to_a".to_string();
            z
        }]);
        assert!(b_to_a.evaluate(&track_frame(10.0, 0.0, 1.0, 1)).is_empty());
        let far_side_frame = RadarFrame {
            timestamp_ns: 0,
            detections: RadarDetections::Tracks(vec![RadarTrack {
                id: 1,
                range_m: 25.0,
                azimuth_deg: 0.0,
                velocity_mps: 1.0,
                rcs_dbsm: 8.0,
                class_hint: None,
            }]),
        };
        assert!(
            b_to_a.evaluate(&far_side_frame).is_empty(),
            "crossing the wrong direction must never fire"
        );
    }

    #[test]
    fn line_cross_either_direction_fires_both_ways() {
        let mut either = RadarAnalyzer::new(&[{
            let mut z = zone("gate", "line_cross", vec![[-10.0, 20.0], [10.0, 20.0]]);
            z.direction = "either".to_string();
            z
        }]);
        assert!(either.evaluate(&track_frame(10.0, 0.0, 1.0, 1)).is_empty());
        let far_side_frame = RadarFrame {
            timestamp_ns: 0,
            detections: RadarDetections::Tracks(vec![RadarTrack {
                id: 1,
                range_m: 25.0,
                azimuth_deg: 0.0,
                velocity_mps: 1.0,
                rcs_dbsm: 8.0,
                class_hint: None,
            }]),
        };
        assert_eq!(either.evaluate(&far_side_frame).len(), 1);
        // Cross back the other way — "either" must fire again.
        assert_eq!(either.evaluate(&track_frame(10.0, 0.0, -1.0, 1)).len(), 1);
    }

    #[test]
    fn loiter_fires_only_after_dwell_and_once_per_visit() {
        let mut analyzer = RadarAnalyzer::new(&[{
            let mut z = zone(
                "dock",
                "loiter",
                vec![[-5.0, 5.0], [5.0, 5.0], [5.0, 20.0], [-5.0, 20.0]],
            );
            z.dwell_s = 0; // fires immediately on first tick inside for a deterministic test
            z
        }]);
        let inside = track_frame(10.0, 0.0, 0.0, 1);
        let first = analyzer.evaluate(&inside);
        assert_eq!(first.len(), 1, "dwell_s=0 should fire on first entry");
        let second = analyzer.evaluate(&inside);
        assert!(
            second.is_empty(),
            "must not refire every frame while still inside"
        );
    }

    #[test]
    fn clutter_suppresses_a_stationary_repeated_return() {
        let mut analyzer = RadarAnalyzer::new(&[zone(
            "hazard",
            "presence",
            vec![[-5.0, 5.0], [5.0, 5.0], [5.0, 20.0], [-5.0, 20.0]],
        )]);
        let stationary = track_frame(10.0, 0.0, 0.0, 1); // velocity 0 = static reflector
        let mut last_events_len = 0;
        for _ in 0..(CLUTTER_CONSECUTIVE_FRAMES + 5) {
            last_events_len = analyzer.evaluate(&stationary).len();
        }
        assert_eq!(
            last_events_len, 0,
            "a static reflector held in place must eventually be suppressed as clutter"
        );
    }

    #[test]
    fn moving_target_is_never_treated_as_clutter() {
        let mut analyzer = RadarAnalyzer::new(&[zone(
            "hazard",
            "presence",
            vec![[-5.0, 5.0], [5.0, 5.0], [5.0, 20.0], [-5.0, 20.0]],
        )]);
        for _ in 0..(CLUTTER_CONSECUTIVE_FRAMES + 5) {
            let events = analyzer.evaluate(&track_frame(10.0, 0.0, 3.0, 1));
            assert_eq!(
                events.len(),
                1,
                "a genuinely moving target must never be suppressed"
            );
        }
    }

    #[test]
    fn point_in_polygon_matches_known_cases() {
        let square = vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(point_in_polygon(5.0, 5.0, &square));
        assert!(!point_in_polygon(50.0, 50.0, &square));
    }

    #[test]
    fn side_of_line_is_consistent_and_detects_a_change() {
        let a = (-10.0, 20.0);
        let b = (10.0, 20.0);
        let near = side_of_line(a, b, (0.0, 10.0));
        let far = side_of_line(a, b, (0.0, 30.0));
        assert_ne!(near, far);
    }
}
