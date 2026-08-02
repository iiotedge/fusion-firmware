// src/stream/rtsp_server.rs
//
// RTSP serving: owns the GStreamer RTSP server, its GLib main loop thread,
// and the appsrc that the capture pipeline feeds. Which encoder runs inside
// the media pipeline is decided entirely by stream::encoder::plan_encoder —
// this module never names a codec or vendor element.
use crate::config::{CameraConfig, StreamConfig};
use crate::core::error::{EdgeError, EdgeResult};
use crate::stream::encoder;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_rtsp_server::prelude::*;
use gstreamer_rtsp_server::{RTSPAuth, RTSPMediaFactory, RTSPServer, RTSPToken};

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tracing::{debug, info, warn};

/// Soft cap on frames queued inside appsrc before we start dropping at the
/// push site (flow control via need-data/enough-data, supported on every
/// GStreamer version we target). Kept small deliberately: every buffered
/// frame here is added end-to-end latency, not just a safety margin — 2
/// frames is enough slack to absorb normal push-thread jitter without
/// piling up a visible delay.
const APPSRC_QUEUE_FRAMES: u64 = 2;

/// Depth of the `queue` element between appsrc and the encoder. This is a
/// thread hand-off boundary (decouples the Rust push thread from the
/// encoder thread), not a second buffering stage on top of
/// `APPSRC_QUEUE_FRAMES` — kept equally small for the same reason.
const ENCODE_QUEUE_FRAMES: u32 = 2;

pub struct RtspStreamer {
    /// Live appsrc of the currently prepared media, if any client is
    /// connected. Swapped by the media-configure callback on the GLib thread.
    appsrc: Arc<Mutex<Option<gst_app::AppSrc>>>,
    /// True while appsrc signals enough-data (encoder can't keep up); frames
    /// are dropped at the push site instead of ballooning RAM.
    congested: Arc<AtomicBool>,
    main_loop: gst::glib::MainLoop,
    endpoint: String,
}

impl RtspStreamer {
    /// `text_overlays` is the pre-built overlay fragment from
    /// stream::overlay::text_fragment ("" = none) — injected after
    /// videoconvert so pango elements see raw video. `audio` adds a second
    /// RTP stream (pay1) fed by the shared microphone fanout.
    pub fn new(
        camera: &CameraConfig,
        stream: &StreamConfig,
        text_overlays: &str,
        audio: Option<&crate::audio::AudioPlan>,
        access: &crate::security::AccessControl,
    ) -> EdgeResult<Self> {
        // Homebrew's forked gst-plugin-scanner is prone to hanging the very
        // first registry scan on macOS dev machines; scan in-process instead
        // (the default behavior on Windows). Not needed on Linux targets.
        #[cfg(target_os = "macos")]
        std::env::set_var("GST_REGISTRY_FORK", "no");

        gst::init().map_err(|e| EdgeError::StreamFault(format!("gstreamer init: {e}")))?;

        let plan = encoder::plan_encoder(stream)?;
        let (caps, decode_fragment) = crate::media::source_caps(camera);

        let frame_bytes = u64::from(camera.width) * u64::from(camera.height) * 2;
        let audio_branch = audio.map(|plan| plan.rtsp_branch.as_str()).unwrap_or("");
        let launch = format!(
            "( appsrc name=vidsrc is-live=true format=time do-timestamp=true block=false \
             max-bytes={max_bytes} caps=\"{caps}\" \
             ! queue max-size-buffers={queue_frames} leaky=downstream \
             ! {decode_fragment}videoconvert \
             ! {text_overlays}{encode} ! {pay}{audio_branch} )",
            max_bytes = frame_bytes * APPSRC_QUEUE_FRAMES,
            queue_frames = ENCODE_QUEUE_FRAMES,
            encode = plan.encode_fragment,
            pay = plan.rtp_pay_fragment,
        );
        debug!(launch = %launch, "RTSP media pipeline");

        let server = RTSPServer::new();
        server.set_service(&stream.rtsp_port.to_string());

        let mounts = server
            .mount_points()
            .ok_or_else(|| EdgeError::StreamFault("RTSP server has no mount points".into()))?;

        let factory = RTSPMediaFactory::new();
        factory.set_launch(&launch);
        // One shared pipeline feeds every connected client.
        factory.set_shared(true);
        // GStreamer's own RTSP server default (200ms) is the single biggest
        // hidden source of "stream delay" on an otherwise well-tuned
        // pipeline; config-driven so a lossy link can raise it back up
        // instead of only ever going one direction from a hardcoded 0.
        factory.set_latency(stream.rtsp_latency_ms);

        // Access control: with enforcement on, the factory requires the
        // "viewer" role and every configured user is registered for HTTP
        // Basic auth. Without it, the stream stays open (warned at boot).
        if access.rtsp_enforced() {
            configure_rtsp_auth(&server, &factory, access)?;
            info!(users = access.users().len(), "RTSP authentication enforced");
        }

        let appsrc: Arc<Mutex<Option<gst_app::AppSrc>>> = Arc::new(Mutex::new(None));
        let congested = Arc::new(AtomicBool::new(false));

        let appsrc_slot = appsrc.clone();
        let congested_cb = congested.clone();
        let audio_fanout = audio.map(|plan| plan.fanout.clone());
        factory.connect_media_configure(move |_, media| {
            let bin = match media.element().downcast::<gst::Bin>() {
                Ok(bin) => bin,
                Err(_) => {
                    warn!("RTSP media is not a bin; cannot attach appsrc");
                    return;
                }
            };
            let Some(element) = bin.by_name_recurse_up("vidsrc") else {
                warn!("RTSP media pipeline has no 'vidsrc' appsrc element");
                return;
            };
            let Ok(src) = element.downcast::<gst_app::AppSrc>() else {
                warn!("'vidsrc' element is not an appsrc");
                return;
            };

            // Flow control: mark congestion instead of queueing unboundedly.
            let congested_need = congested_cb.clone();
            let congested_enough = congested_cb.clone();
            src.set_callbacks(
                gst_app::AppSrcCallbacks::builder()
                    .need_data(move |_, _| congested_need.store(false, Ordering::Relaxed))
                    .enough_data(move |_| congested_enough.store(true, Ordering::Relaxed))
                    .build(),
            );

            *appsrc_slot.lock() = Some(src);

            // Attach this media's audio input to the microphone fanout.
            if let Some(fanout) = &audio_fanout {
                match bin
                    .by_name_recurse_up("audsrc")
                    .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
                {
                    Some(audio_src) => fanout.add(audio_src),
                    None => warn!("audio configured but RTSP media has no 'audsrc'"),
                }
            }
            info!("RTSP client connected; media pipeline prepared");
        });

        mounts.add_factory(&stream.rtsp_path, factory);

        server
            .attach(None)
            .map_err(|e| EdgeError::StreamFault(format!("RTSP server attach: {e}")))?;

        let main_loop = gst::glib::MainLoop::new(None, false);
        let run_loop = main_loop.clone();
        thread::Builder::new()
            .name("rtsp_mainloop".to_string())
            .spawn(move || run_loop.run())
            .map_err(|e| EdgeError::StreamFault(format!("spawn rtsp mainloop: {e}")))?;

        let endpoint = format!("rtsp://0.0.0.0:{}{}", stream.rtsp_port, stream.rtsp_path);
        info!(endpoint = %endpoint, encoder = %plan.element, codec = ?plan.codec, "RTSP server ready");

        Ok(Self {
            appsrc,
            congested,
            main_loop,
            endpoint,
        })
    }

    /// Feed one frame to connected RTSP clients. Takes a refcounted
    /// gst::Buffer so the media thread can share one frame allocation between
    /// this and the chunk recorder. Cheap no-op when nobody is watching;
    /// drops frames under congestion rather than block the capture thread.
    pub fn push(&self, buffer: gst::Buffer, frame_id: u64) -> EdgeResult<()> {
        let mut slot = self.appsrc.lock();
        let Some(appsrc) = slot.as_ref() else {
            return Ok(()); // no client connected
        };

        if self.congested.load(Ordering::Relaxed) {
            debug!("Encoder congested; dropping frame {frame_id}");
            return Ok(());
        }

        if let Err(flow) = appsrc.push_buffer(buffer) {
            // Flushing means the last client tore the pipeline down between
            // our lock and the push — expected during disconnects.
            info!("RTSP pipeline stopped accepting frames ({flow:?}); detaching appsrc");
            *slot = None;
        }
        Ok(())
    }

    #[allow(dead_code)] // consumed by the ONVIF media service in onvif::services
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for RtspStreamer {
    fn drop(&mut self) {
        self.main_loop.quit();
    }
}

/// HTTP Basic auth on the RTSP server: the media factory is restricted to the
/// "viewer" role (access + construct), and each configured user is registered
/// with a token carrying that role.
fn configure_rtsp_auth(
    server: &RTSPServer,
    factory: &RTSPMediaFactory,
    access: &crate::security::AccessControl,
) -> EdgeResult<()> {
    use gstreamer_rtsp_server::RTSP_TOKEN_MEDIA_FACTORY_ROLE;

    const ROLE: &str = "viewer";

    // Only holders of the ROLE token may access/construct this factory's media.
    factory.add_role_from_structure(
        &gst::Structure::builder(ROLE)
            .field("media.factory.access", true)
            .field("media.factory.construct", true)
            .build(),
    );

    let auth = RTSPAuth::new();
    for user in access.users() {
        let token = RTSPToken::builder()
            .field(RTSP_TOKEN_MEDIA_FACTORY_ROLE.as_str(), ROLE)
            .build();
        let basic = RTSPAuth::make_basic(&user.username, &user.password);
        auth.add_basic(&basic, &token);
    }
    server.set_auth(Some(&auth));
    Ok(())
}
