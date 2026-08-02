// src/hal/gst_v4l2.rs
//
// GStreamer-backed V4L2 capture (`v4l2src ! appsink`). Unlike the raw
// generic_v4l2 backend this handles multi-planar capture nodes
// (V4L2_CAP_VIDEO_CAPTURE_MPLANE) — which is what Rockchip's rkisp1 exposes
// on the Radxa Zero 3E — as well as UVC webcams and compressed (MJPG)
// formats, at the cost of going through GStreamer's negotiation instead of
// raw mmap ioctls.
use crate::config::CameraConfig;
use crate::core::error::{EdgeError, EdgeResult};
use crate::hal::{FrameHandle, VideoSource, FRAME_POOL_SIZE};
use crate::media;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

pub struct GstV4l2Camera {
    device_node: String,
    source_params: String,
    caps: String,
    pipeline: Option<gst::Pipeline>,
    appsink: Option<gst_app::AppSink>,
    frame_pool: Vec<Vec<u8>>,
    pool_cursor: usize,
    frame_counter: u64,
}

impl GstV4l2Camera {
    pub fn new(cfg: &CameraConfig) -> Self {
        let caps = media::capture_caps(cfg);
        let mut source_params = cfg.source_params.trim().to_string();
        if !source_params.is_empty() {
            source_params.insert(0, ' ');
        }
        Self {
            device_node: cfg.device_node.clone(),
            source_params,
            caps,
            pipeline: None,
            appsink: None,
            frame_pool: (0..FRAME_POOL_SIZE).map(|_| Vec::new()).collect(),
            pool_cursor: 0,
            frame_counter: 0,
        }
    }

    /// Collect pending error messages off the bus so hardware failures show
    /// the driver's actual complaint instead of a generic state-change error.
    fn drain_bus_errors(pipeline: &gst::Pipeline) -> String {
        let Some(bus) = pipeline.bus() else {
            return "no bus".to_string();
        };
        let mut reasons = Vec::new();
        while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(err) = msg.view() {
                let source = msg
                    .src()
                    .map(|s| s.name().to_string())
                    .unwrap_or_else(|| "?".into());
                let debug = err
                    .debug()
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "no debug info".into());
                reasons.push(format!("[{source}] {}: {debug}", err.error()));
            }
        }
        if reasons.is_empty() {
            "no error details on bus".to_string()
        } else {
            reasons.join("; ")
        }
    }
}

impl VideoSource for GstV4l2Camera {
    fn initialize(&mut self) -> EdgeResult<()> {
        gst::init().map_err(|e| EdgeError::HardwareFault(format!("gstreamer init: {e}")))?;

        // max-buffers=2, drop=true: appsink holds at most 2 frames and drops
        // the oldest under backpressure rather than queueing — capture-side
        // buffering is latency added before anything downstream (AI/encode)
        // even sees the frame, so it's kept to the minimum that still
        // absorbs normal dequeue-thread jitter.
        let launch = format!(
            "v4l2src device={}{} ! {} ! appsink name=sink sync=false max-buffers=2 drop=true",
            self.device_node, self.source_params, self.caps
        );
        info!(pipeline = %launch, "GStreamer V4L2 capture pipeline");

        let pipeline = gst::parse::launch(&launch)
            .map_err(|e| EdgeError::HardwareFault(format!("capture pipeline parse: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| EdgeError::HardwareFault("capture pipeline is not a Pipeline".into()))?;

        let appsink = pipeline
            .by_name("sink")
            .ok_or_else(|| EdgeError::HardwareFault("appsink 'sink' missing".into()))?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| EdgeError::HardwareFault("'sink' is not an appsink".into()))?;

        self.pipeline = Some(pipeline);
        self.appsink = Some(appsink);
        Ok(())
    }

    fn start_stream(&mut self) -> EdgeResult<()> {
        let pipeline = self
            .pipeline
            .as_ref()
            .ok_or_else(|| EdgeError::HardwareFault("start_stream before initialize".into()))?;

        pipeline.set_state(gst::State::Playing).map_err(|_| {
            EdgeError::HardwareFault(format!(
                "capture pipeline refused to start: {}",
                Self::drain_bus_errors(pipeline)
            ))
        })?;

        // Fail fast with the driver's own error (bad node, busy device,
        // unsupported caps) instead of hanging on the first frame.
        let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(5));
        if result.is_err() {
            let reason = Self::drain_bus_errors(pipeline);
            let _ = pipeline.set_state(gst::State::Null);
            return Err(EdgeError::HardwareFault(format!(
                "capture pipeline failed to reach PLAYING on {}: {}",
                self.device_node, reason
            )));
        }

        info!(device = %self.device_node, "GStreamer V4L2 capture running");
        Ok(())
    }

    fn dequeue_frame(&mut self) -> EdgeResult<FrameHandle> {
        let appsink = self
            .appsink
            .as_ref()
            .ok_or_else(|| EdgeError::HardwareFault("dequeue before initialize".into()))?;

        let sample = appsink.pull_sample().map_err(|_| {
            let reason = self
                .pipeline
                .as_ref()
                .map(Self::drain_bus_errors)
                .unwrap_or_default();
            EdgeError::HardwareFault(format!("capture stream ended: {reason}"))
        })?;

        let buffer = sample
            .buffer()
            .ok_or_else(|| EdgeError::HardwareFault("sample without buffer".into()))?;
        let map = buffer
            .map_readable()
            .map_err(|_| EdgeError::MemoryMapFailed)?;

        let cursor = self.pool_cursor;
        self.pool_cursor = (self.pool_cursor + 1) % FRAME_POOL_SIZE;
        let slot = &mut self.frame_pool[cursor];
        slot.clear();
        slot.extend_from_slice(map.as_slice());

        self.frame_counter += 1;
        let timestamp_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        Ok(FrameHandle {
            id: self.frame_counter,
            data_ptr: slot.as_ptr(),
            size: slot.len(),
            timestamp_ns,
        })
    }

    fn stop_stream(&mut self) -> EdgeResult<()> {
        if let Some(pipeline) = self.pipeline.take() {
            if pipeline.set_state(gst::State::Null).is_err() {
                warn!("Capture pipeline did not shut down cleanly");
            }
        }
        self.appsink = None;
        info!("GStreamer V4L2 capture stopped");
        Ok(())
    }
}
