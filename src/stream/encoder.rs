// src/stream/encoder.rs
//
// Encoder *planning*: turns the [stream] + [camera] config into a concrete
// GStreamer launch fragment. All vendor-specific element knowledge (Rockchip
// MPP, NXP VPU, VideoToolbox, software encoders) is confined to this module
// and the config candidate lists — nothing platform-specific leaks into
// main.rs or the RTSP server.
use crate::config::StreamConfig;
use crate::core::error::{EdgeError, EdgeResult};

use gstreamer as gst;
use gstreamer::prelude::*;

use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "h264" | "avc" => Some(Codec::H264),
            "h265" | "hevc" => Some(Codec::H265),
            _ => None,
        }
    }

    /// Stream parser for this codec (needed before both RTP payloaders and
    /// container muxers).
    fn parser(&self) -> &'static str {
        match self {
            Codec::H264 => "h264parse",
            Codec::H265 => "h265parse",
        }
    }

    /// RTP payloader fragment. `config-interval=-1` re-sends SPS/PPS/VPS with
    /// every IDR so RTSP clients can join mid-stream.
    fn rtp_pay_fragment(&self) -> &'static str {
        match self {
            Codec::H264 => "rtph264pay name=pay0 pt=96 config-interval=-1",
            Codec::H265 => "rtph265pay name=pay0 pt=96 config-interval=-1",
        }
    }

    /// Encoding token used by ONVIF media profiles.
    #[allow(dead_code)] // consumed by the ONVIF media service
    pub fn onvif_name(&self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
        }
    }
}

/// A fully resolved encode chain, split so consumers compose their own tail:
/// RTSP appends the RTP payloader, the chunk recorder appends a muxer sink.
pub struct EncoderPlan {
    pub element: String,
    pub codec: Codec,
    /// `<encoder+props> ! <parser>` — codec-agnostic encode front-end.
    pub encode_fragment: String,
    /// `rtpXpay name=pay0 …` — RTSP-specific tail.
    pub rtp_pay_fragment: &'static str,
}

/// Elements whose `bitrate` property is in bits/s rather than the common
/// kbit/s. Checked before generic property probing.
const BITRATE_IN_BPS: &[&str] = &["openh264enc"];

/// GOP-length property names in probe order — encoders disagree on naming.
const GOP_PROPERTIES: &[&str] = &["gop-size", "gop", "key-int-max", "max-keyframe-interval"];

/// Low-latency defaults per element, applied before (and therefore
/// overridable by) the user's `encoder_params`.
fn tuning_defaults(element: &str) -> &'static str {
    match element {
        "x264enc" => "tune=zerolatency speed-preset=ultrafast",
        "x265enc" => "tune=zerolatency speed-preset=ultrafast",
        "vtenc_h264" | "vtenc_h265" => "realtime=true allow-frame-reordering=false",
        _ => "",
    }
}

/// Resolve the encoder element for the configured codec: an explicit
/// `stream.encoder` is preferred, then the codec's candidate list in order,
/// taking the first element registered in this device's GStreamer registry.
pub fn plan_encoder(cfg: &StreamConfig) -> EdgeResult<EncoderPlan> {
    let codec = Codec::parse(&cfg.codec).ok_or_else(|| {
        EdgeError::StreamFault(format!("unsupported stream.codec '{}'", cfg.codec))
    })?;

    let candidates = match codec {
        Codec::H264 => &cfg.encoder_candidates_h264,
        Codec::H265 => &cfg.encoder_candidates_h265,
    };

    let explicit = (!cfg.encoder.is_empty() && cfg.encoder != "auto").then_some(&cfg.encoder);
    let mut search: Vec<&str> = Vec::new();
    if let Some(name) = explicit {
        search.push(name);
    }
    search.extend(candidates.iter().map(String::as_str));

    let element = search
        .iter()
        .find(|name| gst::ElementFactory::find(name).is_some())
        .copied()
        .ok_or_else(|| {
            EdgeError::StreamFault(format!(
                "no {:?} encoder found on this device (searched: {})",
                codec,
                search.join(", ")
            ))
        })?;

    if let Some(name) = explicit {
        if name != element {
            warn!(
                requested = %name,
                selected = %element,
                "Configured encoder not present in GStreamer registry; fell back to candidate list"
            );
        }
    }

    let props = encoder_properties(element, cfg);
    let encode_fragment = format!("{element}{props} ! {parser}", parser = codec.parser());

    info!(encoder = %element, codec = ?codec, fragment = %encode_fragment, "Encoder plan resolved");

    Ok(EncoderPlan {
        element: element.to_string(),
        codec,
        encode_fragment,
        rtp_pay_fragment: codec.rtp_pay_fragment(),
    })
}

/// Build the encoder's property assignments by probing which property names
/// this element actually exposes. User `encoder_params` come last so they
/// override anything generated here.
fn encoder_properties(element: &str, cfg: &StreamConfig) -> String {
    let mut props = String::new();

    match gst::ElementFactory::make(element).build() {
        Ok(probe) => {
            if probe.find_property("bps").is_some() {
                // Rockchip MPP encoders take bits/s via `bps`.
                props.push_str(&format!(" bps={}", u64::from(cfg.bitrate_kbps) * 1000));
            } else if probe.find_property("bitrate").is_some() {
                let value = if BITRATE_IN_BPS.contains(&element) {
                    u64::from(cfg.bitrate_kbps) * 1000
                } else {
                    u64::from(cfg.bitrate_kbps)
                };
                props.push_str(&format!(" bitrate={value}"));
            } else {
                warn!(
                    encoder = %element,
                    "Encoder exposes no bitrate property; use stream.encoder_params for rate control"
                );
            }

            if let Some(gop_prop) = GOP_PROPERTIES
                .iter()
                .find(|p| probe.find_property(p).is_some())
            {
                props.push_str(&format!(" {}={}", gop_prop, cfg.gop_size));
            }
        }
        Err(e) => warn!(encoder = %element, "Could not instantiate encoder for probing: {e}"),
    }

    let tuning = tuning_defaults(element);
    if !tuning.is_empty() {
        props.push_str(&format!(" {tuning}"));
    }
    if !cfg.encoder_params.trim().is_empty() {
        props.push_str(&format!(" {}", cfg.encoder_params.trim()));
    }
    props
}

// NOTE: source-format-to-caps mapping lives in crate::media::source_caps so
// capture backends (hal::gst_v4l2) and this streaming layer share one truth.
