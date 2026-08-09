// src/cluster/fusion.rs
//
// Cross-device AI detection fusion (F9 Phase C): the same physical object
// often crosses multiple camera FOVs in a short window — a person walking
// from the loading-dock camera's view into the warehouse-floor camera's
// view. When two devices report the same label within `tolerance_ms` of
// each other, that's corroborating evidence of one real-world event, not
// two disconnected ones. Mirrors correlation.rs's match + time-tolerance
// pairing pattern, but matches this device's own recent AI detections
// against a peer's `ai_event` cluster broadcast instead of southbound
// machine data.
//
// Deliberately works only with what's already on the wire — Event{kind:
// "ai_event", summary: label} — the compact one-label-per-batch summary
// the AI engine thread already broadcasts to keep the cluster bus light
// (see main.rs's comment on that call site, driven by the BLE transport's
// small payload budget). No wire protocol change needed for this phase.
//
// Radar zone fusion (TODO.md Phase 17e, added 2026-08-09): the same
// principle generalized to a second modality — a radar zone event on one
// mesh node corroborating another node's radar zone event (or, once a
// physical radar exists to test cross-modal matching against, a camera
// AI detection) is one real-world object crossing overlapping coverage,
// not two disconnected ones. `record_local_radar`/`correlate_peer_radar`
// below are genuinely additive: `record_local`/`correlate_peer` (camera
// AI fusion) are untouched, same signatures, same behavior, and radar
// events live in their own queue rather than sharing `recent` — a radar
// zone named e.g. "gate" must never accidentally same-string-match an
// unrelated AI class label. Cross-modal matching (a radar zone
// corroborating a camera AI label, rather than two same-modality events)
// is real, flagged, deliberately NOT attempted here — matching
// "modality X's event summary" against "modality Y's event summary" needs
// its own design (what counts as "the same object" between a class label
// and a zone name?), not a guess bolted onto this pass. `Event.kind`
// already carries `"radar_zone"` as a distinct value from `"ai_event"`
// (no wire protocol change — MessageKind::Event's `kind` field is
// already free-form), so a caller dispatches to the right queue purely
// from data already on the wire.
use crate::config::ClusterFusionConfig;

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bound on remembered local detections. Stale entries fall out of
/// `tolerance_ms` naturally; this just caps worst-case memory if this
/// device is detecting continuously and nothing prunes the queue for a
/// while.
const CAPACITY: usize = 32;

pub struct DetectionFusion {
    tolerance: Duration,
    recent: Mutex<VecDeque<(String, Instant)>>,
    /// Radar zone-event fusion (Phase 17e) — see this module's header
    /// comment for why it's a separate queue from `recent` above.
    recent_radar: Mutex<VecDeque<(String, Instant)>>,
}

impl DetectionFusion {
    /// `None` when disabled — the mesh (and the AI engine thread's
    /// `record_local` calls) runs with zero fusion overhead.
    pub fn new(cfg: &ClusterFusionConfig) -> Option<Arc<Self>> {
        if !cfg.enabled {
            return None;
        }
        Some(Arc::new(Self {
            tolerance: Duration::from_millis(cfg.tolerance_ms),
            recent: Mutex::new(VecDeque::with_capacity(CAPACITY)),
            recent_radar: Mutex::new(VecDeque::with_capacity(CAPACITY)),
        }))
    }

    /// Record one of this device's own AI detections — called from the AI
    /// engine thread at the same point it broadcasts the label as a
    /// cluster `ai_event`, so both sides of a future correlation always
    /// compare the same label string.
    pub fn record_local(&self, label: &str) {
        let mut recent = self.recent.lock();
        recent.push_back((label.to_string(), Instant::now()));
        while recent.len() > CAPACITY {
            recent.pop_front();
        }
    }

    /// A peer just reported `label`. If this device saw the same label
    /// recently enough to plausibly be the same real-world object (not
    /// just the same class showing up hours apart), returns how long ago —
    /// `None` means no match, not "no data".
    pub fn correlate_peer(&self, label: &str) -> Option<Duration> {
        let mut recent = self.recent.lock();
        let now = Instant::now();
        recent.retain(|(_, seen_at)| now.duration_since(*seen_at) <= self.tolerance);
        recent
            .iter()
            .rev()
            .find(|(l, _)| l == label)
            .map(|(_, seen_at)| now.duration_since(*seen_at))
    }

    /// Record one of this device's own radar zone events — called from
    /// the radar analytics thread at the same point it broadcasts the
    /// zone name as a cluster `radar_zone` event, mirroring
    /// `record_local`'s exact shape for camera AI detections.
    pub fn record_local_radar(&self, zone_name: &str) {
        let mut recent = self.recent_radar.lock();
        recent.push_back((zone_name.to_string(), Instant::now()));
        while recent.len() > CAPACITY {
            recent.pop_front();
        }
    }

    /// A peer just reported a `radar_zone` event named `zone_name`. Same
    /// matching semantics as `correlate_peer`, against the radar-only
    /// queue.
    pub fn correlate_peer_radar(&self, zone_name: &str) -> Option<Duration> {
        let mut recent = self.recent_radar.lock();
        let now = Instant::now();
        recent.retain(|(_, seen_at)| now.duration_since(*seen_at) <= self.tolerance);
        recent
            .iter()
            .rev()
            .find(|(z, _)| z == zone_name)
            .map(|(_, seen_at)| now.duration_since(*seen_at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fusion(tolerance_ms: u64) -> Arc<DetectionFusion> {
        DetectionFusion::new(&ClusterFusionConfig {
            enabled: true,
            tolerance_ms,
        })
        .expect("enabled")
    }

    #[test]
    fn disabled_config_yields_none() {
        assert!(DetectionFusion::new(&ClusterFusionConfig {
            enabled: false,
            tolerance_ms: 3000,
        })
        .is_none());
    }

    #[test]
    fn matches_recent_same_label() {
        let f = fusion(3000);
        f.record_local("person");
        let age = f.correlate_peer("person");
        assert!(age.is_some());
        assert!(age.unwrap() < Duration::from_millis(100));
    }

    #[test]
    fn does_not_match_different_label() {
        let f = fusion(3000);
        f.record_local("person");
        assert!(f.correlate_peer("car").is_none());
    }

    #[test]
    fn does_not_match_once_outside_tolerance() {
        let f = fusion(10); // 10ms window — trivially expires
        f.record_local("person");
        std::thread::sleep(Duration::from_millis(50));
        assert!(f.correlate_peer("person").is_none());
    }

    #[test]
    fn no_local_detections_never_matches() {
        let f = fusion(3000);
        assert!(f.correlate_peer("person").is_none());
    }

    #[test]
    fn capacity_bound_keeps_only_the_most_recent_entries() {
        let f = fusion(60_000); // long tolerance so nothing expires by age
        for i in 0..(CAPACITY + 10) {
            f.record_local(&format!("label-{i}"));
        }
        assert_eq!(f.recent.lock().len(), CAPACITY);
        // Oldest entries were evicted; the earliest label must be gone.
        assert!(f.correlate_peer("label-0").is_none());
        // The most recent one is still there.
        assert!(f
            .correlate_peer(&format!("label-{}", CAPACITY + 9))
            .is_some());
    }

    #[test]
    fn radar_zone_fusion_matches_recent_same_zone() {
        let f = fusion(3000);
        f.record_local_radar("gate");
        let age = f.correlate_peer_radar("gate");
        assert!(age.is_some());
        assert!(age.unwrap() < Duration::from_millis(100));
    }

    #[test]
    fn radar_zone_fusion_does_not_match_different_zone() {
        let f = fusion(3000);
        f.record_local_radar("gate");
        assert!(f.correlate_peer_radar("haul_road").is_none());
    }

    #[test]
    fn radar_zone_fusion_is_independent_of_camera_ai_fusion() {
        // A radar zone and an AI label happening to share a name must
        // never cross-match — separate queues, separate identities.
        let f = fusion(3000);
        f.record_local("gate");
        assert!(
            f.correlate_peer_radar("gate").is_none(),
            "an AI label must never satisfy a radar zone correlation"
        );
        f.record_local_radar("gate");
        assert!(
            f.correlate_peer("gate").is_some(),
            "record_local's own 'gate' label is still there and must still match correlate_peer"
        );
    }

    #[test]
    fn radar_zone_fusion_does_not_match_once_outside_tolerance() {
        let f = fusion(10);
        f.record_local_radar("gate");
        std::thread::sleep(Duration::from_millis(50));
        assert!(f.correlate_peer_radar("gate").is_none());
    }
}
