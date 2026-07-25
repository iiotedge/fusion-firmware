// src/core/watchdog.rs
//
// Thread-liveness watchdog (wires the previously-dead [watchdog] config).
//
// The capture-loop supervisor already catches a worker thread that *panics*
// and exits (JoinHandle::is_finished). This catches the other failure mode: a
// worker still alive but wedged — a GStreamer pipeline deadlock, an NPU call
// that never returns. Each worker bumps a heartbeat every loop iteration; if
// any heartbeat goes stale beyond `thread_timeout_ms`, the process exits(2)
// so the init system restarts it into a clean state.
//
// Timeout tuning: the analytics thread runs inference inline, so the timeout
// must exceed the worst-case per-frame time (CPU YOLO can spike to hundreds
// of ms). The default is deliberately generous; tighten per deployment.
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A named heartbeat a worker bumps to prove liveness.
pub struct Heartbeat {
    last_beat_ms: AtomicU64,
    started: Instant,
}

impl Heartbeat {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Called by the worker each loop iteration.
    pub fn beat(&self) {
        let now = self.now_ms();
        self.last_beat_ms.store(now, Ordering::Relaxed);
    }

    fn stale_for_ms(&self) -> u64 {
        self.now_ms()
            .saturating_sub(self.last_beat_ms.load(Ordering::Relaxed))
    }
}

/// Registry of worker heartbeats plus the configured timeout.
#[derive(Clone)]
pub struct Watchdog {
    inner: Arc<WatchdogInner>,
}

struct WatchdogInner {
    started: Instant,
    timeout_ms: u64,
    beats: Mutex<Vec<(String, Arc<Heartbeat>)>>,
}

impl Watchdog {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            inner: Arc::new(WatchdogInner {
                started: Instant::now(),
                timeout_ms,
                beats: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Register a worker; the returned heartbeat is bumped from its loop.
    pub fn register(&self, name: &str) -> Arc<Heartbeat> {
        let hb = Arc::new(Heartbeat {
            last_beat_ms: AtomicU64::new(self.inner.started.elapsed().as_millis() as u64),
            started: self.inner.started,
        });
        self.inner.beats.lock().push((name.to_string(), hb.clone()));
        hb
    }

    /// The first worker whose heartbeat exceeds the timeout, if any.
    /// Disabled (returns None) when timeout_ms == 0.
    pub fn stalled_worker(&self) -> Option<(String, u64)> {
        if self.inner.timeout_ms == 0 {
            return None;
        }
        self.inner.beats.lock().iter().find_map(|(name, hb)| {
            let stale = hb.stale_for_ms();
            (stale > self.inner.timeout_ms).then(|| (name.clone(), stale))
        })
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.inner.timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stale_heartbeat() {
        let wd = Watchdog::new(50);
        let hb = wd.register("worker");
        hb.beat();
        assert!(wd.stalled_worker().is_none());
        std::thread::sleep(Duration::from_millis(80));
        let stalled = wd.stalled_worker().expect("should be stale");
        assert_eq!(stalled.0, "worker");
        // A fresh beat clears it.
        hb.beat();
        assert!(wd.stalled_worker().is_none());
    }

    #[test]
    fn zero_timeout_disables() {
        let wd = Watchdog::new(0);
        let _hb = wd.register("worker");
        std::thread::sleep(Duration::from_millis(10));
        assert!(wd.stalled_worker().is_none());
    }
}
