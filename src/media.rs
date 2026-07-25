// src/media.rs
//
// Shared pixel-format knowledge: maps the config's camera format string to
// GStreamer caps. Lives outside both hal/ and stream/ because capture
// backends (hal::gst_v4l2) and the RTSP pipeline (stream::rtsp_server) need
// the same mapping and neither layer should depend on the other.
use crate::config::CameraConfig;
use crate::core::error::{EdgeError, EdgeResult};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::str::FromStr;
use tracing::warn;

/// GStreamer caps for the camera's output plus an optional decode fragment.
/// Compressed sensor formats (MJPG) need a decoder ahead of videoconvert;
/// raw formats map to their GStreamer names.
pub fn source_caps(camera: &CameraConfig) -> (String, &'static str) {
    let up = camera.format.to_uppercase();
    if up == "MJPG" || up == "JPEG" {
        return (
            format!(
                "image/jpeg,width={},height={},framerate={}/1",
                camera.width, camera.height, camera.fps
            ),
            "jpegdec ! ",
        );
    }

    let gst_format = raw_gst_format(&up);
    (
        format!(
            "video/x-raw,format={},width={},height={},framerate={}/1",
            gst_format, camera.width, camera.height, camera.fps
        ),
        "",
    )
}

/// Caps for negotiating with a *real capture device*: format and size only.
/// The sensor's current mode dictates the actual frame interval — pinning
/// `framerate` here makes rkisp-style ISP drivers fail caps negotiation
/// ("streaming stopped, reason not-negotiated (-4)") because they report
/// frame intervals differently than the requested exact fraction.
// Only the Linux-gated hal::gst_v4l2 backend captures from real devices.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn capture_caps(camera: &CameraConfig) -> String {
    let up = camera.format.to_uppercase();
    if up == "MJPG" || up == "JPEG" {
        return format!("image/jpeg,width={},height={}", camera.width, camera.height);
    }
    format!(
        "video/x-raw,format={},width={},height={}",
        raw_gst_format(&up),
        camera.width,
        camera.height
    )
}

/// Encode one raw camera frame to JPEG via a one-shot GStreamer pipeline
/// (appsrc ! videoconvert ! jpegenc ! appsink). Built per call — snapshots
/// happen at event rates, not frame rates, so simplicity wins over a
/// persistent pipeline.
pub fn encode_jpeg(frame: &[u8], camera: &CameraConfig, quality: u32) -> EdgeResult<Vec<u8>> {
    gst::init().map_err(|e| EdgeError::StreamFault(format!("gstreamer init: {e}")))?;
    if gst::ElementFactory::find("jpegenc").is_none() {
        return Err(EdgeError::StreamFault(
            "jpegenc element missing (install GStreamer plugins-good)".into(),
        ));
    }

    let caps = capture_caps(camera);
    let launch = format!(
        "appsrc name=src ! {caps} ! videoconvert ! jpegenc quality={} ! appsink name=sink",
        quality.clamp(10, 100)
    );
    let pipeline = gst::parse::launch(&launch)
        .map_err(|e| EdgeError::StreamFault(format!("snapshot pipeline parse: {e}")))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| EdgeError::StreamFault("snapshot pipeline is not a Pipeline".into()))?;

    let appsrc = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .ok_or_else(|| EdgeError::StreamFault("snapshot appsrc missing".into()))?;
    appsrc
        .set_caps(Some(&gst::Caps::from_str(&caps).map_err(|e| {
            EdgeError::StreamFault(format!("snapshot caps: {e}"))
        })?));
    let appsink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| EdgeError::StreamFault("snapshot appsink missing".into()))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| EdgeError::StreamFault("snapshot pipeline refused to start".into()))?;

    let result = (|| {
        appsrc
            .push_buffer(gst::Buffer::from_slice(frame.to_vec()))
            .map_err(|e| EdgeError::StreamFault(format!("snapshot push: {e:?}")))?;
        let _ = appsrc.end_of_stream();
        let sample = appsink
            .try_pull_sample(gst::ClockTime::from_seconds(3))
            .ok_or_else(|| EdgeError::StreamFault("snapshot encode timed out".into()))?;
        let buffer = sample
            .buffer()
            .ok_or_else(|| EdgeError::StreamFault("snapshot sample without buffer".into()))?;
        let map = buffer
            .map_readable()
            .map_err(|_| EdgeError::MemoryMapFailed)?;
        Ok(map.as_slice().to_vec())
    })();

    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn raw_gst_format(up: &str) -> &'static str {
    match up {
        "YUYV" | "YUY2" => "YUY2",
        "NV12" => "NV12",
        "UYVY" => "UYVY",
        "I420" | "YU12" => "I420",
        "RGB" | "RGB3" => "RGB",
        "BGR" | "BGR3" => "BGR",
        "GREY" | "GRAY8" => "GRAY8",
        other => {
            warn!(
                format = %other,
                "Unknown camera.format; assuming YUY2 — extend media::raw_gst_format if this is a real sensor format"
            );
            "YUY2"
        }
    }
}
