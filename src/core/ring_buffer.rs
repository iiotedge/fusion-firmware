// src/core/ring_buffer.rs
use crate::core::error::{EdgeError, EdgeResult};
use crate::hal::FrameHandle;
use crossbeam::channel::{bounded, Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;
use tracing::warn;

/// Minimum spacing between repeated "queue full" warnings for the same
/// queue. A stalled consumer can otherwise flood the log (and journald/disk
/// I/O) at full capture fps; this collapses a burst into one line with a
/// dropped-frame count.
const QUEUE_WARN_INTERVAL_MS: u64 = 1000;

/// Rate-limited drop counter for one queue: logs at most once per
/// `QUEUE_WARN_INTERVAL_MS`, folding in how many frames were dropped since
/// the last line. `route_frame` is only ever called from the capture
/// thread, but takes `&self`, so this uses atomics rather than `&mut`.
#[derive(Default)]
struct DropWarner {
    dropped_since_log: AtomicU32,
    last_log_ms: AtomicU64,
}

impl DropWarner {
    /// Returns Some(dropped_count) when a warning should be logged now.
    fn note_drop(&self, epoch: &Instant) -> Option<u32> {
        let dropped = self.dropped_since_log.fetch_add(1, Ordering::Relaxed) + 1;
        let now_ms = epoch.elapsed().as_millis() as u64;
        let last = self.last_log_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < QUEUE_WARN_INTERVAL_MS {
            return None;
        }
        if self
            .last_log_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            self.dropped_since_log.store(0, Ordering::Relaxed);
            Some(dropped)
        } else {
            None
        }
    }
}

pub struct FrameRouter {
    ai_tx: Sender<FrameHandle>,
    stream_tx: Sender<FrameHandle>,
    // Third, optional consumer for the Matter WebRTC live H.264 tap
    // (src/matter/encoder.rs) — `None` whenever `[matter].enabled = false`,
    // so a disabled/absent feature costs nothing here beyond one `Option`
    // check per frame. Same drop-on-full, best-effort semantics as
    // `stream_tx`: a stalled or absent Matter viewer must never affect
    // AI/RTSP delivery.
    matter_tx: Option<Sender<FrameHandle>>,
    started: Instant,
    ai_drops: DropWarner,
    stream_drops: DropWarner,
    matter_drops: DropWarner,
}

pub struct FrameReceivers {
    pub ai_rx: Receiver<FrameHandle>,
    pub stream_rx: Receiver<FrameHandle>,
    /// `Some` only when `FrameRouter::new` was asked to wire up the Matter
    /// tap — see `FrameRouter::with_matter_tap`.
    pub matter_rx: Option<Receiver<FrameHandle>>,
}

impl FrameRouter {
    /// Creates a bounded channel. In Industry 4.0, if the queue hits capacity,
    /// we drop the frame rather than exhausting system RAM.
    pub fn new(capacity: usize) -> (Self, FrameReceivers) {
        let (ai_tx, ai_rx) = bounded(capacity);
        let (stream_tx, stream_rx) = bounded(capacity);

        (
            Self {
                ai_tx,
                stream_tx,
                matter_tx: None,
                started: Instant::now(),
                ai_drops: DropWarner::default(),
                stream_drops: DropWarner::default(),
                matter_drops: DropWarner::default(),
            },
            FrameReceivers {
                ai_rx,
                stream_rx,
                matter_rx: None,
            },
        )
    }

    /// Adds the Matter live-encode consumer queue to an already-built
    /// router/receivers pair (same `capacity` as the AI/stream queues —
    /// see `config::validate`'s `queue_capacity` vs. `FRAME_POOL_SIZE`
    /// margin check, which bounds each queue independently and so already
    /// covers a third queue at the same depth).
    pub fn with_matter_tap(mut self, receivers: FrameReceivers, capacity: usize) -> (Self, FrameReceivers) {
        let (matter_tx, matter_rx) = bounded(capacity);
        self.matter_tx = Some(matter_tx);
        (
            self,
            FrameReceivers {
                matter_rx: Some(matter_rx),
                ..receivers
            },
        )
    }

    /// Broadcasts the memory pointer to all worker pipelines
    pub fn route_frame(&self, frame: FrameHandle) -> EdgeResult<()> {
        // Send to Security Stream (High Priority - Drop if full to keep latency low)
        if let Err(TrySendError::Full(_)) = self.stream_tx.try_send(frame.clone()) {
            if let Some(dropped) = self.stream_drops.note_drop(&self.started) {
                warn!(dropped, "Security Stream queue full! Dropping frames.");
            }
        }

        // Send to the Matter WebRTC live-encode tap, if wired up. Best-effort,
        // same as the stream queue — an unreachable/stalled Matter session
        // must not affect AI or RTSP delivery.
        if let Some(matter_tx) = &self.matter_tx {
            if let Err(TrySendError::Full(_)) = matter_tx.try_send(frame.clone()) {
                if let Some(dropped) = self.matter_drops.note_drop(&self.started) {
                    warn!(dropped, "Matter encode queue full! Dropping frames.");
                }
            }
        }

        // Send to AI Engine (Drop if full, better to skip a frame than crash NPU)
        if let Err(TrySendError::Full(_)) = self.ai_tx.try_send(frame) {
            if let Some(dropped) = self.ai_drops.note_drop(&self.started) {
                warn!(dropped, "AI Engine queue full! Dropping frames.");
            }
            return Err(EdgeError::QueueFull);
        }

        Ok(())
    }
}
