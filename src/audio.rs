// src/audio.rs
//
// Microphone support (F1 audio): one capture pipeline feeds every consumer,
// mirroring the video architecture — RTSP clients get an RTP audio stream
// (pay1) and NVR chunks get an audio track, from the same samples.
//
//   {source} ! audioconvert ! audioresample ! S16LE caps ! appsink
//        └─► AudioFanout ─► RTSP media appsrc (per client pipeline)
//                        └─► recorder appsrc (splitmuxsink audio pad)
//
// The source is config-driven: "auto" resolves to autoaudiosrc (ALSA on
// Linux, CoreAudio on macOS); any explicit GStreamer fragment works too —
// "alsasrc device=hw:1,0" for a specific mic, "audiotestsrc is-live=true"
// for a bench tone. Codec (opus/aac) and encoder element are probed like the
// video encoder candidates. Audio failures never touch the video path.
use crate::config::AudioConfig;
use crate::core::error::{EdgeError, EdgeResult};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use parking_lot::Mutex;
use std::sync::Arc;
use std::thread;
use tracing::{info, warn};

/// Fan-out of raw audio buffers to the currently attached pipeline inputs.
/// Sinks that stop accepting (client disconnected, recorder stopped) are
/// dropped on the next push.
#[derive(Clone, Default)]
pub struct AudioFanout {
    sinks: Arc<Mutex<Vec<gst_app::AppSrc>>>,
}

impl AudioFanout {
    pub fn add(&self, appsrc: gst_app::AppSrc) {
        self.sinks.lock().push(appsrc);
    }

    fn push(&self, buffer: &gst::Buffer) {
        self.sinks
            .lock()
            .retain(|sink| sink.push_buffer(buffer.clone()).is_ok());
    }
}

/// Everything the media pipelines need to carry audio.
pub struct AudioPlan {
    pub fanout: AudioFanout,
    /// Extra launch branch for the RTSP media bin (`... ! pay1`).
    pub rtsp_branch: String,
    /// Extra launch branch for the recorder (`... ! recsink.audio_0`).
    pub recorder_branch: String,
    _pipeline: gst::Pipeline,
}

/// Build and start audio capture. None when disabled or when the source /
/// encoder cannot start (with the reason logged) — video runs regardless.
pub fn start(cfg: &AudioConfig) -> Option<AudioPlan> {
    if !cfg.enabled {
        return None;
    }
    match try_start(cfg) {
        Ok(plan) => Some(plan),
        Err(e) => {
            warn!("Audio disabled: {e}");
            None
        }
    }
}

fn try_start(cfg: &AudioConfig) -> EdgeResult<AudioPlan> {
    gst::init().map_err(|e| EdgeError::StreamFault(format!("gstreamer init: {e}")))?;

    let caps = raw_caps(cfg);
    let (encoder, rtp_pay) = encoder_fragment(cfg)?;
    let source = if cfg.source.trim().is_empty() || cfg.source.trim() == "auto" {
        "autoaudiosrc".to_string()
    } else {
        cfg.source.trim().to_string()
    };

    let launch = format!(
        "{source} ! audioconvert ! audioresample ! {caps} \
         ! appsink name=asink sync=false max-buffers=16 drop=true"
    );
    let pipeline = gst::parse::launch(&launch)
        .map_err(|e| EdgeError::StreamFault(format!("audio pipeline parse: {e}")))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| EdgeError::StreamFault("audio capture is not a Pipeline".into()))?;
    let appsink = pipeline
        .by_name("asink")
        .ok_or_else(|| EdgeError::StreamFault("appsink 'asink' missing".into()))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| EdgeError::StreamFault("'asink' is not an appsink".into()))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| EdgeError::StreamFault(format!("audio source '{source}' refused to start")))?;
    let (result, _, _) = pipeline.state(gst::ClockTime::from_seconds(5));
    if result.is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(EdgeError::StreamFault(format!(
            "audio source '{source}' failed to reach PLAYING (is a microphone available?)"
        )));
    }

    let fanout = AudioFanout::default();
    let worker = fanout.clone();
    thread::Builder::new()
        .name("audio_capture".to_string())
        .spawn(move || {
            while let Ok(sample) = appsink.pull_sample() {
                let Some(buffer) = sample.buffer() else {
                    continue;
                };
                let Ok(map) = buffer.map_readable() else {
                    continue;
                };
                worker.push(&gst::Buffer::from_slice(map.as_slice().to_vec()));
            }
            warn!("Audio capture ended (source EOS or error); video continues without sound");
        })
        .map_err(|e| EdgeError::StreamFault(format!("spawn audio thread: {e}")))?;

    info!(source = %source, codec = %cfg.codec, "Audio capture running");

    let appsrc_common = format!(
        "appsrc name={{name}} is-live=true format=time do-timestamp=true block=false caps=\"{caps}\" \
         ! queue max-size-buffers=32 leaky=downstream ! audioconvert"
    );
    Ok(AudioPlan {
        fanout,
        rtsp_branch: format!(
            " {} ! {encoder} ! {rtp_pay}",
            appsrc_common.replace("{name}", "audsrc")
        ),
        recorder_branch: format!(
            " {} ! {encoder} ! queue ! recsink.audio_0",
            appsrc_common.replace("{name}", "audrec")
        ),
        _pipeline: pipeline,
    })
}

fn raw_caps(cfg: &AudioConfig) -> String {
    format!(
        "audio/x-raw,format=S16LE,rate={},channels={},layout=interleaved",
        cfg.sample_rate.max(8000),
        cfg.channels.clamp(1, 2),
    )
}

/// Probe the encoder for the configured codec. Opus ships in plugins-base
/// everywhere; AAC candidates cover libav/vo-aacenc/fdk/faac builds.
fn encoder_fragment(cfg: &AudioConfig) -> EdgeResult<(String, &'static str)> {
    let bps = u64::from(cfg.bitrate_kbps.max(16)) * 1000;
    match cfg.codec.to_lowercase().as_str() {
        "opus" => Ok((
            format!("opusenc bitrate={bps}"),
            "rtpopuspay name=pay1 pt=97",
        )),
        "aac" => {
            let candidates = ["avenc_aac", "voaacenc", "fdkaacenc", "faac"];
            let element = candidates
                .iter()
                .find(|name| gst::ElementFactory::find(name).is_some())
                .ok_or_else(|| {
                    EdgeError::StreamFault(format!(
                        "no AAC encoder on this device (searched: {}); use audio.codec = \"opus\"",
                        candidates.join(", ")
                    ))
                })?;
            Ok((
                format!("{element} bitrate={bps}"),
                "rtpmp4gpay name=pay1 pt=97",
            ))
        }
        other => Err(EdgeError::StreamFault(format!(
            "unsupported audio.codec '{other}' (opus | aac)"
        ))),
    }
}
