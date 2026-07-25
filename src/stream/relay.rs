// src/stream/relay.rs
//
// Cloud-push relay (stream_start/stream_stop over the existing MQTT command
// channel, src/commands.rs): the firmware's own RTSP server is LAN-only
// pull (clients connect directly to the camera). This is a separate,
// additive path — on command, an in-process GStreamer pipeline pulls that
// same local feed and republishes it, unmodified (never re-encoded), to an
// external RTSP ingest endpoint (media-ingestion-service's MediaMTX; see
// that service's own README for the full command contract this
// implements).
//
// Same lifecycle pattern as every other pipeline in this firmware
// (RtspStreamer, ChunkRecorder, GstV4l2Camera): build a launch string,
// gst::parse::launch it, own the Pipeline, drive its state via
// set_state(). No subprocess, no ffmpeg — the frames never leave the
// process; rtspclientsink re-payloads and pushes them using the same
// GStreamer runtime already linked in for everything else.
//
// Status: implemented against the documented rtspclientsink element
// (gst-plugins-good >= 1.20). `set_state(Playing)` succeeding only means the
// pipeline was *accepted* — `rtspclientsink` connects to the remote server
// asynchronously, so a real failure (auth rejected, connection refused,
// remote hangup) shows up later as an ERROR message on the pipeline's bus,
// not as an Err from `start()`. `watch_bus` below exists specifically to
// catch that: found missing during a real field deployment (2026-07-25)
// where `stream_start` acked "ok" and logged "Cloud relay started" while
// the app's HLS playback kept failing with zero corresponding error in this
// firmware's own logs — there was no code path that could have logged one.
use crate::config::StreamConfig;

use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use tracing::{info, warn};

pub struct StreamRelay {
    // "h264" | "h265" (config aliases "avc"/"hevc" already normalized by
    // the caller) — the relay never transcodes, so it must depay/parse
    // whatever the local RTSP server is actually encoding.
    codec: String,
    pipeline: Mutex<Option<gst::Pipeline>>,
    /// Bumped on every start()/stop(). Lets a `watch_bus` thread recognize
    /// its pipeline has already been superseded (a newer stream_start, or a
    /// stream_stop) and exit quietly instead of tearing down whatever is
    /// current when its own (possibly stale, possibly delayed) error
    /// arrives.
    generation: AtomicU64,
}

impl StreamRelay {
    pub fn new(stream_cfg: &StreamConfig) -> Self {
        Self {
            codec: stream_cfg.codec.to_lowercase(),
            pipeline: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// (Re)starts the relay. Any existing pipeline is torn down first, so a
    /// repeated stream_start is a clean restart rather than a rejected
    /// conflict — the platform's own session reaper can issue an
    /// unsolicited stream_stop/stream_start at any time (heartbeat timeout,
    /// session TTL), so idempotency here matters more than detecting a
    /// "duplicate" start.
    ///
    /// `local_url` and `publish_url` both carry embedded RTSP credentials
    /// (see commands.rs) — never log either one.
    pub fn start(self: Arc<Self>, local_url: &str, publish_url: &str) -> Result<(), String> {
        // Bump first: invalidates any watcher still bound to a prior
        // pipeline before that pipeline is torn down below.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.stop_locked();

        if gst::ElementFactory::find("rtspclientsink").is_none() {
            return Err(
                "rtspclientsink element missing (install gstreamer1.0-plugins-good/bad \
                 with RTSP client-sink support)"
                    .into(),
            );
        }
        let (depay, parse) = match self.codec.as_str() {
            "h265" | "hevc" => ("rtph265depay", "h265parse"),
            _ => ("rtph264depay", "h264parse"),
        };
        // latency=200: small jitter buffer on the pull side; config-interval=1
        // re-inserts SPS/PPS periodically so a fresh MediaMTX viewer can
        // sync without waiting for the next natural keyframe.
        let launch = format!(
            "rtspsrc location=\"{local_url}\" protocols=tcp latency=200 ! {depay} ! \
             {parse} config-interval=1 ! rtspclientsink location=\"{publish_url}\" protocols=tcp"
        );
        let pipeline = gst::parse::launch(&launch)
            .map_err(|e| format!("relay pipeline parse: {e}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "relay pipeline is not a Pipeline".to_string())?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "relay pipeline refused to start".to_string())?;

        info!("Cloud relay started");
        let watch_pipeline = pipeline.clone();
        *self.pipeline.lock() = Some(pipeline);

        let relay = Arc::clone(&self);
        let spawned = thread::Builder::new()
            .name("cloud_relay_bus".to_string())
            .spawn(move || relay.watch_bus(watch_pipeline, generation));
        if let Err(e) = spawned {
            warn!("Cloud relay: failed to spawn bus watcher ({e}) — pipeline errors after this point won't be logged");
        }
        Ok(())
    }

    /// Blocks on this pipeline's bus until it errors, reaches EOS
    /// unexpectedly, or a newer start()/stop() supersedes it (checked every
    /// 5s via the poll timeout — `bus.timed_pop_filtered` has no "wake on
    /// external event" variant, so this is a bounded poll, not a busy loop).
    fn watch_bus(&self, pipeline: gst::Pipeline, generation: u64) {
        let Some(bus) = pipeline.bus() else {
            warn!("Cloud relay: pipeline has no bus, cannot watch for connection errors");
            return;
        };
        loop {
            if self.generation.load(Ordering::SeqCst) != generation {
                return; // superseded by a newer start()/stop() — not our pipeline anymore
            }
            let msg = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Error, gst::MessageType::Eos],
            );
            let Some(msg) = msg else { continue };
            match msg.view() {
                gst::MessageView::Error(err) => {
                    let source = msg
                        .src()
                        .map(|s| s.name().to_string())
                        .unwrap_or_else(|| "?".into());
                    let debug_info = err
                        .debug()
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "no debug info".into());
                    warn!(
                        source = %source,
                        error = %err.error(),
                        debug = %debug_info,
                        "Cloud relay pipeline error — publish to media-ingestion-service \
                         failed or dropped; tearing down (issue stream_start again to retry)"
                    );
                    self.stop_if_current(generation);
                    return;
                }
                gst::MessageView::Eos(_) => {
                    info!("Cloud relay pipeline reached end-of-stream unexpectedly; tearing down");
                    self.stop_if_current(generation);
                    return;
                }
                _ => {}
            }
        }
    }

    fn stop_if_current(&self, generation: u64) {
        if self.generation.load(Ordering::SeqCst) == generation {
            self.stop_locked();
        }
    }

    /// Stops the relay if running; a no-op (not an error) if it wasn't —
    /// stream_stop must be safe to receive unsolicited, at any time.
    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.stop_locked();
    }

    fn stop_locked(&self) {
        if let Some(pipeline) = self.pipeline.lock().take() {
            let _ = pipeline.set_state(gst::State::Null);
            info!("Cloud relay stopped");
        }
    }
}
