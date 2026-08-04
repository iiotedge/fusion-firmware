// src/ai/rules.rs
//
// Customizable AI detection rules (TODO.md Phase 16): zone presence, line
// crossing, loitering — the same small vocabulary Axis Object Analytics,
// Frigate NVR and ONVIF Profile M all converge on. Strictly downstream of
// ai/parser.rs's decode+NMS: this module never touches raw model output,
// only the same `AiEvent` list overlay/telemetry/cluster-fusion already
// consume. An independent analyzer, mirroring how tamper.rs/motion.rs stay
// separate from the shared capture/inference pipeline rather than
// modifying it.
//
// Point-in-zone convention (confirmed via Frigate's docs — this is *the*
// standard, not one implementation's arbitrary choice): a detection's
// **bounding-box bottom-center**, not its centroid — approximates the
// object's ground-contact point, so "inside the fenced area" means the
// intuitive thing for a person/vehicle instead of firing the instant the
// TOP of their box clips the zone edge.
//
// No multi-frame object tracker exists anywhere in ai/ today (parser.rs is
// single-frame NMS only) — line-crossing and loitering both need *some*
// notion of "is this the same object as last frame" to detect a direction
// change or measure dwell time. Rather than build real tracking (SORT/
// ByteTrack-style ID assignment — a genuinely separate, larger lift), this
// uses the simplest viable approach TODO.md's design calls for: a coarse
// (class, position-bucket) key. Two same-class detections landing in the
// same bucket on consecutive frames are treated as "probably the same
// object" — good enough for a single, slow-moving subject per bucket;
// multiple objects of the same class crossing paths in one bucket can
// confuse it. Upgrade to real tracking only if that proves too
// false-positive-prone in practice.
use crate::ai::engine::AiEvent;
use crate::config::{AiConfig, AiRule, CameraConfig};
use crate::schedule::Schedule;

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A rule match. Self-contained (no borrowed references back into
/// `RuleEngine`) so the caller can queue/move it freely — actions are
/// cloned from the rule's own config rather than looked up again.
#[derive(Debug, Clone)]
pub struct RuleEvent {
    pub rule_name: String,
    pub mode: String,
    pub class: String,
    pub confidence: f32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub actions: Vec<String>,
    pub webhook_url: String,
    pub gpio_chip: String,
    pub gpio_line: u32,
    pub gpio_pulse_ms: u64,
}

/// Coarse track key component — see the module doc comment and
/// `line_bucket`. `line_cross` uses the first element (a 1D bucket along
/// the line's length); `loiter` doesn't bucket by position at all and
/// always uses `(0, 0)` (occupancy of the whole zone by a class, not a
/// specific position within it — see that branch's comment in `evaluate`).
type Bucket = (i32, i32);
const BUCKET_FRACTION: f32 = 0.05;

/// A dropped-and-forgotten track doesn't need explicit cleanup logic
/// beyond this TTL — see `prune_stale`.
const TRACK_STALE_AFTER: Duration = Duration::from_secs(5);

struct Track {
    last_seen: Instant,
    /// line_cross: which side of the line this (class, bucket) was last
    /// seen on. `None` until observed once — a rule never fires on the
    /// very first sighting, only on an actual transition.
    side: Option<bool>,
    /// loiter: when this track most recently, uninterruptedly entered the
    /// zone. Cleared the moment it's seen outside the zone.
    entered_at: Option<Instant>,
    /// loiter: already fired for the current uninterrupted dwell — don't
    /// refire every subsequent frame until the object leaves and re-enters.
    fired: bool,
}

impl Track {
    fn fresh(now: Instant) -> Self {
        Self {
            last_seen: now,
            side: None,
            entered_at: None,
            fired: false,
        }
    }
}

struct CompiledRule {
    cfg: AiRule,
    /// Zone/line points scaled from normalized `[0,1]` to pixel space once
    /// at compile time — camera resolution is fixed at boot, so there's no
    /// need to rescale on every frame.
    points_px: Vec<(f32, f32)>,
    schedule: Schedule,
    tracks: HashMap<(String, Bucket), Track>,
}

/// Owned by the analytics thread alongside `MotionDetector`/`TamperDetector`
/// — no internal locking, `evaluate` takes `&mut self` the same way those
/// do, since nothing else ever touches it concurrently.
pub struct RuleEngine {
    rules: Vec<CompiledRule>,
}

impl RuleEngine {
    pub fn new(ai: &AiConfig, camera: &CameraConfig) -> Self {
        let frame_width = camera.width as f32;
        let frame_height = camera.height as f32;
        let rules = ai
            .rules
            .iter()
            .filter(|r| r.enabled)
            .map(|cfg| {
                let points_px = cfg
                    .zone
                    .iter()
                    .map(|[x, y]| (x * frame_width, y * frame_height))
                    .collect();
                CompiledRule {
                    schedule: Schedule::new(&cfg.schedule),
                    cfg: cfg.clone(),
                    points_px,
                    tracks: HashMap::new(),
                }
            })
            .collect();
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Evaluates every currently-armed rule against this frame's
    /// detections (already class/confidence-filtered by the AI engine
    /// upstream — see `AiRule::min_confidence`'s doc comment for why this
    /// module can only ever raise that floor further, never lower it).
    pub fn evaluate(&mut self, detections: &[AiEvent]) -> Vec<RuleEvent> {
        let now = Instant::now();
        let mut fired = Vec::new();

        for rule in &mut self.rules {
            if !rule.schedule.armed_now() {
                continue;
            }
            prune_stale(&mut rule.tracks, now);

            let min_confidence = rule.cfg.min_confidence.unwrap_or(0.0);
            for det in detections {
                if det.confidence < min_confidence {
                    continue;
                }
                if !rule.cfg.classes.is_empty() && !rule.cfg.classes.contains(&det.label) {
                    continue;
                }

                let bx = det.x as f32 + det.w as f32 / 2.0;
                let by = (det.y + det.h) as f32;

                let event = match rule.cfg.mode.as_str() {
                    "presence" => {
                        point_in_polygon(bx, by, &rule.points_px).then(|| new_event(rule, det))
                    }
                    "line_cross" => {
                        // Bucketed by position *along the line*, not raw
                        // (x,y) — see evaluate_line_cross's doc comment for
                        // why a positional 2D bucket doesn't work here.
                        let (a, b) = (rule.points_px[0], rule.points_px[1]);
                        let key = (det.label.clone(), line_bucket(a, b, (bx, by)));
                        evaluate_line_cross(rule, &key, (bx, by), now, det)
                    }
                    // Keyed by class alone, not position: loitering is
                    // about continuous *occupancy of the zone* by this
                    // class, not re-identifying one specific object at one
                    // specific spot — a bucket-keyed track would fragment
                    // across a "leaves from here, re-enters over there"
                    // visit (or even the same spot, if the object drifted
                    // through other buckets on its way out) and never
                    // reset. The already-accepted tradeoff (TODO.md Phase
                    // 16b) is that two simultaneous same-class loiterers
                    // in one zone share a single dwell timer, not that a
                    // single loiterer's own timer fragments by position.
                    "loiter" => {
                        let key = (det.label.clone(), (0, 0));
                        evaluate_loiter(rule, &key, (bx, by), now, det)
                    }
                    // Rejected at config load (config.rs's validate()); a
                    // rule can never reach this with an unknown mode.
                    _ => None,
                };
                if let Some(event) = event {
                    fired.push(event);
                }
            }
        }
        fired
    }
}

/// Buckets a point by its projection *onto the line* (a 1D position along
/// the line's length), not its raw 2D (x,y). A raw positional bucket would
/// put "just before" and "just after" the line in entirely different
/// buckets — exactly the transition this is meant to detect — losing all
/// track continuity the moment an object actually crosses. Projecting onto
/// the line means an object moving roughly perpendicular to it (the normal
/// case for an actual crossing) stays in the same bucket the whole time,
/// while movement *along* the line still buckets separately as intended.
fn line_bucket(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> Bucket {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq < f32::EPSILON {
        0.0
    } else {
        ((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len_sq
    };
    ((t / BUCKET_FRACTION) as i32, 0)
}

fn evaluate_line_cross(
    rule: &mut CompiledRule,
    key: &(String, Bucket),
    point: (f32, f32),
    now: Instant,
    det: &AiEvent,
) -> Option<RuleEvent> {
    let (a, b) = (rule.points_px[0], rule.points_px[1]);
    let side = side_of_line(a, b, point);
    let track = rule
        .tracks
        .entry(key.clone())
        .or_insert_with(|| Track::fresh(now));
    let previous_side = track.side;
    track.side = Some(side);
    track.last_seen = now;

    let previous_side = previous_side?; // no event on a track's first sighting
    if previous_side == side {
        return None; // no transition
    }
    // Arbitrary but fixed convention: false->true is "a_to_b" (the line's
    // first config point to its second), true->false is "b_to_a" — what
    // matters is it's applied consistently, not which literal side is which.
    let crossed_a_to_b = !previous_side && side;
    let direction_matches = match rule.cfg.direction.as_str() {
        "a_to_b" => crossed_a_to_b,
        "b_to_a" => !crossed_a_to_b,
        "either" => true,
        _ => false, // rejected at config load
    };
    direction_matches.then(|| new_event(rule, det))
}

fn evaluate_loiter(
    rule: &mut CompiledRule,
    key: &(String, Bucket),
    point: (f32, f32),
    now: Instant,
    det: &AiEvent,
) -> Option<RuleEvent> {
    let inside = point_in_polygon(point.0, point.1, &rule.points_px);
    let dwell = Duration::from_secs(rule.cfg.dwell_s);
    let track = rule
        .tracks
        .entry(key.clone())
        .or_insert_with(|| Track::fresh(now));
    track.last_seen = now;

    if !inside {
        track.entered_at = None;
        track.fired = false;
        return None;
    }
    let entered_at = *track.entered_at.get_or_insert(now);
    if !track.fired && now.duration_since(entered_at) >= dwell {
        track.fired = true;
        return Some(new_event(rule, det));
    }
    None
}

fn new_event(rule: &CompiledRule, det: &AiEvent) -> RuleEvent {
    RuleEvent {
        rule_name: rule.cfg.name.clone(),
        mode: rule.cfg.mode.clone(),
        class: det.label.clone(),
        confidence: det.confidence,
        x: det.x,
        y: det.y,
        w: det.w,
        h: det.h,
        actions: rule.cfg.actions.clone(),
        webhook_url: rule.cfg.webhook_url.clone(),
        gpio_chip: rule.cfg.gpio_chip.clone(),
        gpio_line: rule.cfg.gpio_line,
        gpio_pulse_ms: rule.cfg.gpio_pulse_ms,
    }
}

fn prune_stale(tracks: &mut HashMap<(String, Bucket), Track>, now: Instant) {
    tracks.retain(|_, t| now.duration_since(t.last_seen) < TRACK_STALE_AFTER);
}

/// Ray-casting point-in-polygon test (even-odd rule) — standard algorithm,
/// correct for any simple (non-self-intersecting) polygon. Self-intersecting
/// zones aren't rejected at config load (see config.rs's validate()) —
/// same pragmatic scope limit `motion.zones`' own rectangle-only validation
/// already accepts, not worth a full geometry-cleanliness check here.
fn point_in_polygon(px: f32, py: f32, polygon: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Which side of the directed line (a -> b) point `p` is on, via the 2D
/// cross product's sign. The boolean's meaning ("left"/"right") isn't
/// itself significant — only that the same physical side always yields the
/// same value, so a change between two calls means the line was crossed.
fn side_of_line(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> bool {
    let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    cross > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ai_cfg(rules: Vec<AiRule>) -> AiConfig {
        AiConfig {
            enabled: true,
            model_path: String::new(),
            runtime: "onnx".to_string(),
            hardware_delegate: String::new(),
            parser: "yolov8".to_string(),
            confidence_threshold: 0.5,
            nms_iou_threshold: 0.45,
            inference_fps_limit: 0,
            roi: Vec::new(),
            input_width: 640,
            input_height: 640,
            labels: Vec::new(),
            class_filter: Vec::new(),
            intra_threads: 1,
            onnx_dylib_path: String::new(),
            test_hooks_enabled: false,
            rules,
        }
    }

    fn camera_cfg() -> CameraConfig {
        CameraConfig {
            r#type: "MOCK".to_string(),
            device_node: String::new(),
            width: 1000,
            height: 1000,
            fps: 15,
            format: "NV12".to_string(),
            auto_exposure: true,
            exposure_time_us: 0,
            gain: 0,
            source_params: String::new(),
        }
    }

    fn presence_rule(zone: Vec<[f32; 2]>) -> AiRule {
        AiRule {
            name: "test_presence".to_string(),
            enabled: true,
            classes: Vec::new(),
            min_confidence: None,
            zone,
            mode: "presence".to_string(),
            direction: String::new(),
            dwell_s: 0,
            schedule: Default::default(),
            actions: Vec::new(),
            webhook_url: String::new(),
            gpio_chip: String::new(),
            gpio_line: 0,
            gpio_pulse_ms: 0,
        }
    }

    fn detection_at(cx: u32, cy_bottom: u32) -> AiEvent {
        // A tiny box whose bottom-center lands exactly at (cx, cy_bottom).
        AiEvent {
            label: "person".to_string(),
            confidence: 0.9,
            x: cx,
            y: cy_bottom - 10,
            w: 0,
            h: 10,
        }
    }

    #[test]
    fn presence_fires_when_bottom_center_enters_polygon() {
        let zone = vec![[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.1, 0.9]];
        let mut engine = RuleEngine::new(&ai_cfg(vec![presence_rule(zone)]), &camera_cfg());

        let outside = engine.evaluate(&[detection_at(50, 50)]);
        assert!(outside.is_empty(), "point outside the zone must not fire");

        let inside = engine.evaluate(&[detection_at(500, 500)]);
        assert_eq!(inside.len(), 1);
        assert_eq!(inside[0].rule_name, "test_presence");
        assert_eq!(inside[0].mode, "presence");
    }

    #[test]
    fn presence_respects_class_filter() {
        let zone = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let mut rule = presence_rule(zone);
        rule.classes = vec!["vehicle".to_string()];
        let mut engine = RuleEngine::new(&ai_cfg(vec![rule]), &camera_cfg());

        let fired = engine.evaluate(&[detection_at(500, 500)]); // label "person"
        assert!(fired.is_empty(), "class not in allowlist must not fire");
    }

    #[test]
    fn presence_respects_min_confidence() {
        let zone = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let mut rule = presence_rule(zone);
        rule.min_confidence = Some(0.95);
        let mut engine = RuleEngine::new(&ai_cfg(vec![rule]), &camera_cfg());

        let mut det = detection_at(500, 500);
        det.confidence = 0.9;
        assert!(engine.evaluate(&[det]).is_empty());

        let mut det = detection_at(500, 500);
        det.confidence = 0.99;
        assert_eq!(engine.evaluate(&[det]).len(), 1);
    }

    #[test]
    fn line_cross_fires_only_on_direction_match() {
        // Vertical line down the middle; direction "a_to_b" only.
        let mut rule = presence_rule(vec![[0.5, 0.0], [0.5, 1.0]]);
        rule.mode = "line_cross".to_string();
        rule.direction = "a_to_b".to_string();
        let mut engine = RuleEngine::new(&ai_cfg(vec![rule]), &camera_cfg());

        // First sighting on one side: never fires (no transition yet).
        assert!(engine.evaluate(&[detection_at(200, 500)]).is_empty());
        // Crosses to the other side: fires (matches whichever direction
        // this transition maps to).
        let first_cross = engine.evaluate(&[detection_at(800, 500)]);
        // Crossing back: the OPPOSITE direction, must not fire again
        // (only one of the two directions is armed).
        let second_cross = engine.evaluate(&[detection_at(200, 500)]);
        assert_ne!(
            first_cross.is_empty(),
            second_cross.is_empty(),
            "exactly one of the two opposite crossings should match a single-direction rule"
        );
    }

    #[test]
    fn line_cross_either_direction_fires_both_ways() {
        let mut rule = presence_rule(vec![[0.5, 0.0], [0.5, 1.0]]);
        rule.mode = "line_cross".to_string();
        rule.direction = "either".to_string();
        let mut engine = RuleEngine::new(&ai_cfg(vec![rule]), &camera_cfg());

        assert!(engine.evaluate(&[detection_at(200, 500)]).is_empty()); // first sighting
        assert_eq!(engine.evaluate(&[detection_at(800, 500)]).len(), 1); // cross 1
        assert_eq!(engine.evaluate(&[detection_at(200, 500)]).len(), 1); // cross back
    }

    #[test]
    fn loiter_fires_only_after_dwell_and_only_once_per_visit() {
        // A central sub-region, not the whole frame -- (50, 50) below needs
        // to land genuinely outside it to exercise "leaves the zone".
        let mut rule = presence_rule(vec![[0.3, 0.3], [0.7, 0.3], [0.7, 0.7], [0.3, 0.7]]);
        rule.mode = "loiter".to_string();
        rule.dwell_s = 0; // fires on the frame it's satisfied, for a fast test
        let mut engine = RuleEngine::new(&ai_cfg(vec![rule]), &camera_cfg());

        let first = engine.evaluate(&[detection_at(500, 500)]);
        assert_eq!(first.len(), 1, "dwell_s=0 should fire on first entry");

        let second = engine.evaluate(&[detection_at(500, 500)]);
        assert!(
            second.is_empty(),
            "must not refire every frame while still inside"
        );

        // Leaves, then re-enters: fires again exactly once.
        engine.evaluate(&[detection_at(50, 50)]);
        let third = engine.evaluate(&[detection_at(500, 500)]);
        assert_eq!(
            third.len(),
            1,
            "leaving and re-entering should allow a fresh fire"
        );
    }

    #[test]
    fn point_in_polygon_matches_known_cases() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        assert!(point_in_polygon(5.0, 5.0, &square));
        assert!(!point_in_polygon(15.0, 5.0, &square));
        assert!(!point_in_polygon(-1.0, 5.0, &square));
    }

    #[test]
    fn side_of_line_is_consistent_and_detects_a_change() {
        let a = (0.0, 0.0);
        let b = (0.0, 10.0);
        let left = side_of_line(a, b, (-5.0, 5.0));
        let right = side_of_line(a, b, (5.0, 5.0));
        assert_ne!(left, right, "opposite sides of the line must differ");
        // Same side queried twice must agree (determinism, not a coin flip).
        assert_eq!(left, side_of_line(a, b, (-1.0, 5.0)));
    }
}
