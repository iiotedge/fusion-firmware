// src/stream/relay.rs
//
// Cloud-push relay (stream_start/stream_stop over the existing MQTT command
// channel, src/commands.rs): the firmware's own RTSP server is LAN-only
// pull (clients connect directly to the camera). This is a separate,
// additive path — on command, an in-process GStreamer pipeline pulls that
// same local feed and republishes it, unmodified (never re-encoded), to an
// external ingest endpoint (media-ingestion-service's MediaMTX; see that
// service's own README for the full command contract this implements).
//
// Same lifecycle pattern as every other pipeline in this firmware
// (RtspStreamer, ChunkRecorder, GstV4l2Camera): build a launch string,
// gst::parse::launch it, own the Pipeline, drive its state via
// set_state(). No subprocess, no ffmpeg — the frames never leave the
// process; the sink element re-payloads and pushes them using the same
// GStreamer runtime already linked in for everything else.
//
// Status: implemented against the documented rtspclientsink element
// (gst-plugins-good >= 1.20). `set_state(Playing)` succeeding only means the
// pipeline was *accepted* — the sink connects to the remote server
// asynchronously, so a real failure (auth rejected, connection refused,
// remote hangup) shows up later as an ERROR message on the pipeline's bus,
// not as an Err from `start()`. `watch_and_reconnect` below exists
// specifically to catch that: found missing during a real field deployment
// (2026-07-25) where `stream_start` acked "ok" and logged "Cloud relay
// started" while the app's HLS playback kept failing with zero
// corresponding error in this firmware's own logs — there was no code path
// that could have logged one.
//
// Auto-reconnect (2026-08-06): the original version of this module tore
// the pipeline down on any error/EOS and stopped — recovery required an
// EXTERNAL stream_start round-trip (something noticing the stream was
// dead, over WAN, then re-issuing the MQTT command), which is the single
// biggest latency cost in "why isn't this back within milliseconds": every
// recovery paid for a full detection-elsewhere + re-command + fresh-
// TCP-connect + RTSP-ANNOUNCE/SETUP/RECORD handshake cycle. This module
// now retries on its own with exponential backoff, and both `rtspsrc` and
// `rtspclientsink` get an explicit `tcp-timeout` well below their 20s
// GStreamer default so a silently-dead connection (dropped NAT mapping,
// black-holed WAN link — no clean RST/FIN either side) is actually
// *noticed* quickly instead of sitting there for 20s before anything even
// starts reconnecting.
//
// WebRTC/WHIP relay mode (2026-08-06): RTSP-over-TCP has an inherent
// multi-round-trip handshake and, worse for a lossy WAN link, TCP's
// head-of-line blocking — one dropped packet stalls delivery of every
// packet behind it until retransmitted, which is exactly wrong for
// real-time video. This is the actual, structural reason "why isn't it
// sub-second like Hikvision/Verkada" — not something a faster retry loop
// can fix. Their systems (and most modern cloud-camera live view: Ring,
// Nest, Verkada, Frigate+go2rtc) use WebRTC for exactly this reason: UDP/
// SRTP transport with NACK/FEC/congestion control designed for lossy
// links, and sub-second ICE-restart reconnects. `mode = "webrtc"` on
// stream_start routes through an RTP payloader + `whipsink` (WHIP =
// WebRTC-HTTP Ingestion Protocol, what MediaMTX's WebRTC ingest speaks)
// instead of
// `rtspclientsink` — same depay/parse head, different tail. This does NOT
// replace the RTSP relay mode: third-party VMS/ONVIF consumers and
// anything not doing its own WebRTC playback still need RTSP, so both
// modes coexist, chosen per stream_start call, not a global switch.
// Corrected 2026-08-08 against a real cross-compiled build of the actual
// element (gst-plugins-rs 0.11.0, matching this fleet's GStreamer 1.22.9):
// the element this module originally targeted, `whipclientsink` with
// `signaller::whip-endpoint`/`signaller::auth-token` child-properties, does
// not exist in any version of gst-plugins-rs that builds against
// GStreamer 1.22 — that whole shape (name, nested `signaller` properties,
// a `video/x-h264`-accepting pad) was never verified against a real
// installed element despite the comment above claiming it was; the
// firmware's `.deb` never actually shipped the plugin at all (a separate,
// previously-known gap — see TODO.md), so this path had silently never
// been exercised. Real `gst-inspect-1.0` output against the actual
// cross-compiled `libgstwebrtchttp.so` on a real device shows the truth:
// the element is named `whipsink`, its `sink_%u` request pad wants
// `application/x-rtp` (an RTP payloader — `rtph264pay`/`rtph265pay` — is
// required upstream of it, same pattern as feeding any RTP-consuming
// sink), and `whip-endpoint`/`auth-token`/`stun-server`/`turn-server` are
// flat top-level properties, not nested under a `signaller` child object
// — `turn-server` is a plain `scheme://user:pass@host:port` string too,
// not the `turn-servers=<"...">` array-literal syntax used below before
// this fix.
use crate::config::{CloudRelayConfig, StreamConfig};

use gstreamer as gst;
use gstreamer::prelude::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Auto-reconnect backoff: starts fast (WAN blips are often momentary —
/// most real recoveries should land within a couple of seconds), doubles
/// on each consecutive failure, caps so a persistently dead link doesn't
/// spin connection attempts forever. Reset to the floor once a pipeline
/// has stayed up past `STABLE_AFTER` — one bad patch shouldn't leave every
/// later reconnect slow for the rest of the session.
const RETRY_BACKOFF_FLOOR: Duration = Duration::from_secs(1);
const RETRY_BACKOFF_CAP: Duration = Duration::from_secs(15);
const STABLE_AFTER: Duration = Duration::from_secs(20);

/// `Fail after timeout microseconds on TCP connections` — the property
/// name/semantics/default (20_000_000 = 20s) are verified against the real
/// elements (`gst-inspect-1.0 rtspsrc`/`rtspclientsink`, gst-plugins-good),
/// not guessed. 5s balances "detect a dead connection fast enough that
/// auto-reconnect is actually worth having" against "don't false-positive
/// and tear down a connection that's merely slow on an ordinary WAN
/// latency spike." RTSP relay mode only — WHIP mode's failure detection is
/// governed by `whipsink`'s own ICE connection-state machine, not
/// this TCP-specific property.
const TCP_TIMEOUT_US: u64 = 5_000_000;

/// Where to push the local feed, and how — chosen per `stream_start` call
/// (see src/commands.rs), not a global config switch, since a VMS/ONVIF
/// consumer on the platform side might need RTSP while a mobile app's live
/// view wants WebRTC's latency, potentially at different times for the
/// same device.
// Debug is derived only so test assertions (`expect_err`, `assert_eq!`)
// can format a value on failure — nothing in production code should ever
// print a RelayTarget with `{:?}`, since both variants carry a secret (see
// each field's own doc comment below). Logging code in this module only
// ever uses `mode_name()`, never the target itself.
#[derive(Clone, Debug)]
pub enum RelayTarget {
    /// `publish_url` carries embedded RTSP credentials (see
    /// `commands.rs::inject_rtsp_credentials`) — never log it.
    Rtsp { publish_url: String },
    /// WHIP's own standard auth mechanism is an `Authorization: Bearer
    /// <auth_token>` header on the signaling POST, not a URL-embedded
    /// credential — never log either field.
    Webrtc {
        whip_url: String,
        auth_token: String,
    },
}

pub struct StreamRelay {
    // "h264" | "h265" (config aliases "avc"/"hevc" already normalized by
    // the caller) — the relay never transcodes, so it must depay/parse
    // whatever the local RTSP server is actually encoding.
    codec: String,
    /// WHIP mode's NAT-traversal config ([cloud_relay].stun_server/
    /// turn_server) — empty = whipsink's own defaults. Unused in
    /// RTSP relay mode.
    stun_server: String,
    turn_server: String,
    pipeline: Mutex<Option<gst::Pipeline>>,
    /// Bumped on every start()/stop() (including internal reconnect
    /// retries — each attempt is its own generation). Lets a background
    /// watcher recognize its pipeline has already been superseded (a newer
    /// stream_start, a stream_stop, or its own next reconnect attempt) and
    /// exit quietly instead of tearing down or reconnecting whatever is
    /// current when its own (possibly stale, possibly delayed) message
    /// arrives.
    generation: AtomicU64,
}

impl StreamRelay {
    pub fn new(stream_cfg: &StreamConfig, cloud_relay_cfg: &CloudRelayConfig) -> Self {
        Self {
            codec: stream_cfg.codec.to_lowercase(),
            stun_server: cloud_relay_cfg.stun_server.clone(),
            turn_server: cloud_relay_cfg.turn_server.clone(),
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
    /// `local_url` carries embedded RTSP credentials (see commands.rs) —
    /// never log it, same caution as `target`'s own fields.
    pub fn start(self: Arc<Self>, local_url: &str, target: RelayTarget) -> Result<(), String> {
        // Bump first: invalidates any watcher still bound to a prior
        // pipeline (or a prior pipeline's in-progress reconnect backoff)
        // before that pipeline is torn down below.
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.stop_locked();

        let pipeline = self.build_and_play(local_url, &target)?;
        info!(mode = target.mode_name(), "Cloud relay started");
        *self.pipeline.lock() = Some(pipeline.clone());

        let relay = Arc::clone(&self);
        let local_url = local_url.to_string();
        let spawned = thread::Builder::new()
            .name("cloud_relay_bus".to_string())
            .spawn(move || relay.watch_and_reconnect(pipeline, generation, local_url, target));
        if let Err(e) = spawned {
            warn!(
                "Cloud relay: failed to spawn bus watcher ({e}) — pipeline errors after this \
                 point won't be logged, and auto-reconnect won't run"
            );
        }
        Ok(())
    }

    /// Builds the launch string and drives one pipeline attempt to
    /// Playing. Doesn't touch `self.pipeline`/`self.generation` — purely
    /// "make one attempt," reused by both the first `start()` call and
    /// every auto-reconnect retry inside `watch_and_reconnect` below.
    fn build_and_play(
        &self,
        local_url: &str,
        target: &RelayTarget,
    ) -> Result<gst::Pipeline, String> {
        let (depay, parse) = match self.codec.as_str() {
            "h265" | "hevc" => ("rtph265depay", "h265parse"),
            _ => ("rtph264depay", "h264parse"),
        };
        // latency=200: small jitter buffer on the pull side; config-interval=1
        // re-inserts SPS/PPS periodically so a fresh viewer can sync
        // without waiting for the next natural keyframe. Shared by both
        // relay modes — only the tail element differs.
        let head = format!(
            "rtspsrc location=\"{local_url}\" protocols=tcp latency=200 tcp-timeout={TCP_TIMEOUT_US} ! \
             {depay} ! {parse} config-interval=1 ! "
        );
        let launch = match target {
            RelayTarget::Rtsp { publish_url } => {
                if gst::ElementFactory::find("rtspclientsink").is_none() {
                    return Err(
                        "rtspclientsink element missing (install gstreamer1.0-plugins-good/bad \
                         with RTSP client-sink support)"
                            .into(),
                    );
                }
                format!(
                    "{head}rtspclientsink location=\"{publish_url}\" protocols=tcp \
                     tcp-timeout={TCP_TIMEOUT_US}"
                )
            }
            RelayTarget::Webrtc {
                whip_url,
                auth_token,
            } => {
                if gst::ElementFactory::find("whipsink").is_none() {
                    return Err(
                        "whipsink element missing (install gst-plugins-rs's webrtchttp plugin \
                         — libgstwebrtchttp.so — for WHIP support)"
                            .into(),
                    );
                }
                // whipsink's request pad (sink_%u) wants application/x-rtp,
                // not the raw parsed video head produces for rtspclientsink
                // above — needs an RTP payloader in between. pt=96: the
                // conventional first dynamic payload type (96-127 range),
                // matching what rtspclientsink/gst-rtsp-server already pick
                // by default elsewhere in this codebase.
                let pay = match self.codec.as_str() {
                    "h265" | "hevc" => "rtph265pay",
                    _ => "rtph264pay",
                };
                let mut tail =
                    format!("{pay} pt=96 ! whipsink whip-endpoint=\"{whip_url}\" auth-token=\"{auth_token}\"");
                if !self.stun_server.is_empty() {
                    tail.push_str(&format!(" stun-server=\"{}\"", self.stun_server));
                }
                if !self.turn_server.is_empty() {
                    // turn-server is a single scheme://user:pass@host:port
                    // string property on whipsink itself, not an array —
                    // confirmed via gst-inspect-1.0 against the real built
                    // element, replacing an earlier, never-verified guess
                    // at gst-plugins-rs's array-literal syntax.
                    tail.push_str(&format!(" turn-server=\"{}\"", self.turn_server));
                }
                format!("{head}{tail}")
            }
        };
        let pipeline = gst::parse::launch(&launch)
            .map_err(|e| format!("relay pipeline parse: {e}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "relay pipeline is not a Pipeline".to_string())?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "relay pipeline refused to start".to_string())?;
        Ok(pipeline)
    }

    /// Owns one `start()` call's whole lifetime: watches the current
    /// pipeline's bus until it errors or hits unexpected EOS, then
    /// auto-reconnects with exponential backoff (rebuilding the pipeline
    /// in place — no new thread per retry, no recursion) until either it
    /// succeeds again or a newer `start()`/`stop()` supersedes this
    /// generation. Intentionally keeps retrying indefinitely rather than
    /// giving up after N attempts: this is a best-effort, opportunistic
    /// layer on top of local recording that's never at risk (see the
    /// module header) — the platform's own session reaper is what decides
    /// when nobody's watching anymore and sends `stream_stop`, not a
    /// retry-count ceiling in here.
    fn watch_and_reconnect(
        self: Arc<Self>,
        mut pipeline: gst::Pipeline,
        generation: u64,
        local_url: String,
        target: RelayTarget,
    ) {
        let mut backoff = RETRY_BACKOFF_FLOOR;
        loop {
            let started_at = Instant::now();
            let Some(bus) = pipeline.bus() else {
                warn!("Cloud relay: pipeline has no bus, cannot watch for connection errors");
                return;
            };

            // --- Watch until a terminal message arrives or we're superseded. ---
            let terminal = loop {
                if self.generation.load(Ordering::SeqCst) != generation {
                    return; // superseded — not our pipeline/generation anymore
                }
                match bus.timed_pop_filtered(
                    gst::ClockTime::from_seconds(5),
                    &[gst::MessageType::Error, gst::MessageType::Eos],
                ) {
                    Some(msg) => break msg,
                    None => continue,
                }
            };
            match terminal.view() {
                gst::MessageView::Error(err) => {
                    let source = terminal
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
                        "Cloud relay pipeline error — reconnecting"
                    );
                }
                gst::MessageView::Eos(_) => {
                    info!("Cloud relay pipeline reached end-of-stream unexpectedly — reconnecting");
                }
                _ => continue, // shouldn't happen given the filter above; not a failure either way
            }

            // Re-check generation right before touching shared state — a
            // stop()/fresh start() could have landed between receiving the
            // message above and here.
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let _ = pipeline.set_state(gst::State::Null);
            *self.pipeline.lock() = None;

            backoff = next_backoff(backoff, started_at.elapsed());

            // --- Backoff, then keep retrying build_and_play until it succeeds or we're superseded. ---
            loop {
                info!(wait_s = backoff.as_secs(), "Cloud relay reconnecting");
                thread::sleep(backoff);
                if self.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                match self.build_and_play(&local_url, &target) {
                    Ok(new_pipeline) => {
                        info!("Cloud relay reconnected");
                        *self.pipeline.lock() = Some(new_pipeline.clone());
                        pipeline = new_pipeline;
                        break; // back to the outer loop's "watch" phase
                    }
                    Err(e) => {
                        warn!(
                            "Cloud relay: reconnect attempt failed to even start ({e}); retrying"
                        );
                        backoff = next_backoff(backoff, Duration::ZERO);
                        continue;
                    }
                }
            }
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

impl RelayTarget {
    fn mode_name(&self) -> &'static str {
        match self {
            RelayTarget::Rtsp { .. } => "rtsp",
            RelayTarget::Webrtc { .. } => "webrtc",
        }
    }
}

/// Pure backoff-escalation policy, split out so it's directly unit-testable
/// without a real GStreamer pipeline: doubles `current` (capped), unless
/// the pipeline that just failed had been up long enough to count as a
/// genuine recovery (`uptime >= STABLE_AFTER`), in which case it resets to
/// the floor — a single earlier bad patch shouldn't leave every later
/// reconnect slow for the rest of the session.
fn next_backoff(current: Duration, uptime: Duration) -> Duration {
    if uptime >= STABLE_AFTER {
        RETRY_BACKOFF_FLOOR
    } else {
        (current * 2).min(RETRY_BACKOFF_CAP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalates_on_a_short_lived_pipeline() {
        assert_eq!(
            next_backoff(RETRY_BACKOFF_FLOOR, Duration::from_secs(3)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(2), Duration::from_secs(3)),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn caps_at_the_ceiling_instead_of_growing_unbounded() {
        assert_eq!(
            next_backoff(RETRY_BACKOFF_CAP, Duration::from_secs(1)),
            RETRY_BACKOFF_CAP
        );
        assert_eq!(
            next_backoff(Duration::from_secs(10), Duration::from_secs(1)),
            RETRY_BACKOFF_CAP
        );
    }

    #[test]
    fn resets_to_the_floor_after_a_stable_run() {
        assert_eq!(
            next_backoff(RETRY_BACKOFF_CAP, STABLE_AFTER),
            RETRY_BACKOFF_FLOOR
        );
        assert_eq!(
            next_backoff(Duration::from_secs(8), Duration::from_secs(9999)),
            RETRY_BACKOFF_FLOOR
        );
    }

    #[test]
    fn zero_uptime_still_escalates_not_resets() {
        // The "reconnect attempt failed to even start" path re-escalates
        // with Duration::ZERO uptime — must NOT be mistaken for "stable."
        assert_eq!(
            next_backoff(RETRY_BACKOFF_FLOOR, Duration::ZERO),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn mode_name_matches_the_stream_start_contract() {
        assert_eq!(
            RelayTarget::Rtsp {
                publish_url: String::new()
            }
            .mode_name(),
            "rtsp"
        );
        assert_eq!(
            RelayTarget::Webrtc {
                whip_url: String::new(),
                auth_token: String::new()
            }
            .mode_name(),
            "webrtc"
        );
    }
}
