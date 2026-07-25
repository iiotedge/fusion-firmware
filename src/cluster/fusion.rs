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
}
