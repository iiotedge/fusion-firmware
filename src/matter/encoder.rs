// src/matter/encoder.rs
//
// Live H.264 tap for the Matter WebRTC Transport Provider (see
// src/matter/camera.rs). This firmware's RTSP encode pipeline
// (src/stream/rtsp_server.rs) owns its GStreamer elements entirely inside
// a `gst_rtsp_server` media factory launch string — there's no external
// tap point exposed on it today — so rather than invasively restructure
// that path, this builds a second, independent, always-on encode
// pipeline: `appsrc ! videoconvert ! <same encoder plan> ! h264parse !
// appsink`, mirroring the exact one-shot pattern `src/media.rs::encode_jpeg`
// already uses for snapshots, just long-running instead of one-shot.
//
// Real trade-off, not hidden: when both RTSP and an active Matter WebRTC
// viewer are running, the camera's raw frames get encoded to H.264
// twice (once per pipeline) rather than one encode being shared between
// both egress paths. Sharing a single encode would mean tapping
// `RtspStreamer`'s internal pipeline, which isn't exposed today — a real
// follow-up (see TODO.md), not attempted in this pass. On the CPU
// encoders this firmware falls back to it's a real cost; on a hardware
// encoder (Rockchip MPP / NXP VPU / VideoToolbox) most devices support
// more than one concurrent encode session, so it's rarely a hard limit.
//
// Frames only reach this pipeline while at least one Matter WebRTC
// session is connected AND the encoder is running — `spawn` starts the
// pipeline unconditionally alongside the Matter node (so the very first
// viewer doesn't wait for a cold start), fed by a THIRD FrameRouter
// consumer queue (see src/core/ring_buffer.rs) that main.rs only wires
// up when `[matter].enabled`.
use crate::config::{CameraConfig, StreamConfig};
use crate::core::error::EdgeResult;
use crate::hal::FrameHandle;
use crate::matter::camera::{H264Frame, LiveH264Source};
use crate::stream::encoder;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crossbeam::channel::Receiver;
use std::sync::Arc;
use std::thread;
use tracing::{error, info, warn};

/// Spawns the encode thread. Returns immediately; the pipeline itself
/// starts inside the thread so a slow GStreamer registry scan can't block
/// the rest of firmware boot (same reasoning as every other subsystem
/// thread in main.rs).
pub fn spawn(
    camera_cfg: CameraConfig,
    stream_cfg: StreamConfig,
    frame_rx: Receiver<FrameHandle>,
    live_source: Arc<LiveH264Source>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    let builder = thread::Builder::new().name("matter_encoder".to_string());
    let spawned = builder.spawn(move || {
        if let Err(e) = run(camera_cfg, stream_cfg, frame_rx, live_source, shutdown) {
            error!("Matter H.264 encode pipeline exited: {e}");
        }
    });
    if let Err(e) = spawned {
        warn!("failed to spawn matter_encoder thread: {e}");
    }
}

fn run(
    camera_cfg: CameraConfig,
    stream_cfg: StreamConfig,
    frame_rx: Receiver<FrameHandle>,
    live_source: Arc<LiveH264Source>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> EdgeResult<()> {
    gst::init().ok(); // idempotent; already initialized by rtsp_server/media on the same process

    let plan = encoder::plan_encoder(&stream_cfg)?;
    let (caps, decode_fragment) = crate::media::source_caps(&camera_cfg);

    // `plan.encode_fragment` already ends in a parser (h264parse/h265parse)
    // tuned for RTSP's own needs; appending a second h264parse with
    // config-interval=-1 here is a harmless pass-through for anything but
    // H.264 in-band parameter-set re-insertion, which WebRTC benefits from
    // (a session that joins after the very first IDR still gets SPS/PPS).
    let launch = format!(
        "appsrc name=src is-live=true format=time do-timestamp=true block=false \
         caps=\"{caps}\" \
         ! {decode_fragment}videoconvert \
         ! {encode} ! h264parse config-interval=-1 \
         ! appsink name=sink emit-signals=true max-buffers=2 drop=true",
        encode = plan.encode_fragment,
    );

    let pipeline = gst::parse::launch(&launch)
        .map_err(|e| {
            crate::core::error::EdgeError::StreamFault(format!("matter encode pipeline parse: {e}"))
        })?
        .downcast::<gst::Pipeline>()
        .map_err(|_| {
            crate::core::error::EdgeError::StreamFault(
                "matter encode pipeline is not a Pipeline".into(),
            )
        })?;

    let appsrc = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| {
            crate::core::error::EdgeError::StreamFault("matter encode appsrc missing".into())
        })?;
    let appsink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| {
            crate::core::error::EdgeError::StreamFault("matter encode appsink missing".into())
        })?;

    let source_for_cb = live_source.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let is_keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
                if let Ok(map) = buffer.map_readable() {
                    source_for_cb.push_frame(H264Frame {
                        data: Arc::from(map.as_slice()),
                        is_keyframe,
                    });
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing).map_err(|_| {
        crate::core::error::EdgeError::StreamFault("matter encode pipeline refused to start".into())
    })?;
    info!(
        "Matter camera: live H.264 encode pipeline started (encoder={})",
        plan.element
    );

    while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
        match frame_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(frame) => {
                // SAFETY: `frame` is a zero-copy handle into the shared
                // capture pool (crate::hal::FrameHandle); it stays valid
                // for `FRAME_POOL_SIZE`-many captures per the same
                // invariant `router.route_frame`'s other consumers rely
                // on (see config::validate's queue_capacity check) — this
                // consumer drains at the receive timeout above, well
                // inside that margin, same as the AI/stream queues.
                let bytes = unsafe { std::slice::from_raw_parts(frame.data_ptr, frame.size) };
                let buffer = gst::Buffer::from_slice(bytes.to_vec());
                if let Err(e) = appsrc.push_buffer(buffer) {
                    warn!("Matter camera: encode pipeline appsrc push failed: {e:?}");
                }
            }
            Err(crossbeam::channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = appsrc.end_of_stream();
    let _ = pipeline.set_state(gst::State::Null);
    info!("Matter camera: live H.264 encode pipeline stopped");
    Ok(())
}
