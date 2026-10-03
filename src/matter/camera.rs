// src/matter/camera.rs
//
// Matter Camera device clusters (Phase 19c): WebRTC Transport Provider
// (0x0553), Camera AV Stream Management, and Zone Management — the real,
// currently-shipping subset of Matter 1.5's camera support (verified
// directly against rs-matter 0.3.0's own installed source and its
// `webrtc_camera` reference example before writing a line of this file;
// see src/matter/mod.rs's header for the full account of that
// verification and why it reversed an earlier, wrong conclusion).
//
// The WebRTC bridging logic below (SDP offer/answer, trickle ICE, the
// per-session str0m driver loop) is a close adaptation of that reference
// example — proven, real code (Apache-2.0, reused under license), not
// written from the raw Matter spec. What's genuinely new here, specific
// to this firmware:
//   - the media source is a LIVE tap of this firmware's own GStreamer
//     H.264 encoder output (an mpsc channel fed by an appsink — see
//     `LiveH264Source` below), not the reference's preloaded static file
//     replayed on a timer;
//   - Camera AV Stream config is sourced from this device's real
//     [camera]/[stream] config instead of hardcoded 1920x1080;
//   - Zone Management pre-seeds this device's actual [[ai.rules]] zones
//     as manufacturer-defined zones (`add_mfg_zone`), reflecting them
//     read-only rather than accepting controller-authored zones — the
//     zones are config-owned by this firmware, not a second source of
//     truth a Matter controller could rewrite.
//
// NOT implemented, deliberately: Camera AV Settings (mechanical/digital
// PTZ). Matter's MPTZ model is absolute-position based (pan/tilt in
// hundredths of a degree, go-to-angle); this firmware's only PTZ backend
// (Pelco-D, src/ptz/) is continuous-move + preset based, with no
// feedback on the head's actual current angle — there is no honest way
// to answer "what angle are you at" or "go to this exact angle" against
// that hardware today. Forcing a mapping anyway (fake absolute angles
// from timed continuous-move guesses) would be exactly the kind of
// unverified claim that produced the WHIP relay bug earlier this
// session. Revisit once there's a PTZ head with real position feedback
// to test against, or once Matter preset-only support (feature bit
// MECHANICAL_PRESETS, independent of MECHANICAL_PAN/TILT/ZOOM) has been
// researched enough to map cleanly onto `PtzController::set_preset`/
// `goto_preset` — see TODO.md Phase 19c.
use crate::config::{AiRule, CameraConfig, StreamConfig};
use crate::ptz::PtzController;

use core::cell::RefCell;
use core::pin::pin;

use std::net::{SocketAddr, UdpSocket};
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender};
use async_executor::LocalExecutor;
use async_io::Async;
use embassy_futures::select::select;

use rs_matter::dm::clusters::app::cam_av_stream::{
    CamAvError, CameraAvStreamConfig, CameraAvStreamHandler, CameraAvStreamHooks,
    Feature as CamAvFeature, RateDistortionPoint, StreamUsageEnum, VideoCodecEnum,
    VideoSensorParams, VideoStream,
};
use rs_matter::dm::clusters::app::webrtc_prov::{
    AnswerOutcome, HandlerAsyncAdaptor as WebRtcAdaptor, OfferParams, OutboundWork, SolicitOutcome,
    WebRtcError, WebRtcHooks, WebRtcProvHandler,
};
use rs_matter::dm::clusters::app::zone_mgmt::{
    Feature as ZoneFeature, HandlerAsyncAdaptor as ZoneMgmtAdaptor, Trigger, Zone, ZoneError,
    ZoneMgmtConfig, ZoneMgmtHandler, ZoneMgmtHooks, ZoneSourceEnum, ZoneTypeEnum, ZoneUseEnum,
};
use rs_matter::dm::clusters::decl::globals::{ICECandidateStruct, WebRTCEndReasonEnum};
use rs_matter::dm::clusters::decl::zone_management::{
    AttributeId as ZoneAttr, CommandId as ZoneCmd,
};
use rs_matter::dm::{Cluster, DeviceType};
use rs_matter::tlv::TLVArray;
use rs_matter::utils::storage::Vec as HVec;
use rs_matter::with;

use str0m::change::SdpOffer;
use str0m::format::Codec as RtcCodec;
use str0m::media::{MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::matter::registry::{ClusterImpl, EndpointSpec};

use tracing::{debug, info, warn};

/// Matter 1.5 "Camera" device type (0x0142, rev 1). Not exposed from
/// rs-matter's `devices` module as a named constant yet — confirmed by
/// grepping the installed 0.3.0 source, not assumed — so defined inline,
/// same as the reference example does. NOTE: 0x0042 is a different
/// device type (Water Valve); using that by mistake makes some
/// controllers (SmartThings included) commission this node as a valve.
pub(crate) const DEV_TYPE_MATTER_CAMERA: DeviceType = DeviceType {
    dtype: 0x0142,
    drev: 1,
};

/// Fixed endpoint id of the camera — the id every previously-paired
/// controller already knows it by.
pub(crate) const CAMERA_ENDPOINT_ID: rs_matter::dm::EndptId = 1;

const N_SESSIONS: usize = 4;
const SDP_LEN: usize = 8 * 1024;
const OUT_LEN: usize = SDP_LEN + 1024;
const CAND_LEN: usize = 256;
const MAX_CAND: usize = 16;
const CAM_AV_NV: usize = 2;
/// Zone table sizing: manufacturer zones only (this firmware's own
/// [[ai.rules]], read-only from a controller's perspective) — generous
/// headroom over what any real [[ai.rules]] list is likely to define.
const ZONE_NZ: usize = 16;
const ZONE_NV: usize = 16;
const ZONE_NT: usize = 16;

pub(crate) type WebRtc =
    WebRtcProvHandler<Str0mHooks, N_SESSIONS, SDP_LEN, OUT_LEN, CAND_LEN, MAX_CAND>;
pub(crate) type CamAv = CameraAvStreamHandler<'static, CamHooks, CAM_AV_NV>;
pub(crate) type ZoneMgmt = ZoneMgmtHandler<ReadOnlyZoneHooks, ZONE_NZ, ZONE_NV, ZONE_NT>;

/// One live H.264 access unit (Annex-B, `00 00 00 01`-prefixed NAL
/// units), handed from the GStreamer tap to whichever WebRTC session is
/// currently active. Cheap to clone (`Arc`-backed payload) since the same
/// frame may need to reach multiple concurrent sessions.
#[derive(Clone)]
pub struct H264Frame {
    pub data: Arc<[u8]>,
    /// Not yet consumed by `session_loop` (a new subscriber just waits for
    /// the next access unit rather than requesting an immediate keyframe)
    /// — kept because it's cheap to compute at the source (encoder.rs
    /// reads it straight off the GStreamer buffer's DELTA_UNIT flag) and
    /// is the natural hook for a real "wait for/force a keyframe on new
    /// session" policy later.
    #[allow(dead_code)]
    pub is_keyframe: bool,
}

/// The live media source this module reads from — a plain broadcast: every
/// active WebRTC session gets its own `std_mpsc` receiver, and
/// `src/matter/encoder.rs`'s real H.264 encode thread calls `push_frame`
/// once per real access unit it produces. Unlike the reference example's
/// `Arc<Vec<H264Frame>>` (a whole file preloaded and replayed on a fixed-
/// fps timer), pacing here comes from the encoder's own real cadence —
/// there is no simulated clock anywhere in this path.
#[derive(Default)]
pub struct LiveH264Source {
    subscribers: std::sync::Mutex<Vec<std_mpsc::Sender<H264Frame>>>,
}

impl LiveH264Source {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn subscribe(&self) -> std_mpsc::Receiver<H264Frame> {
        let (tx, rx) = std_mpsc::channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Fans one real encoded access unit out to every session currently
    /// subscribed. A session that hung up has a disconnected receiver by
    /// now; its sender fails and is pruned here rather than needing its
    /// own cleanup callback.
    pub fn push_frame(&self, frame: H264Frame) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| tx.send(frame.clone()).is_ok());
    }
}

/// Hooks for `CameraAVStreamManagement`. Allocation/modification/
/// deallocation are acknowledged but otherwise no-ops, same honest
/// reasoning as the reference example's `Str0mCamHooks`: the actual
/// H.264 encoder is this firmware's own GStreamer pipeline, already
/// running for RTSP/recording regardless of what the Matter controller
/// thinks it allocated — there's no separate encoder instance for these
/// hooks to actually start or stop.
pub(crate) struct CamHooks;

impl CameraAvStreamHooks for CamHooks {
    async fn allocate_video(&self, stream: &VideoStream) -> Result<(), CamAvError> {
        info!(
            id = stream.video_stream_id,
            w = stream.max_width,
            h = stream.max_height,
            fps = stream.max_frame_rate,
            "Matter camera: allocate video stream"
        );
        Ok(())
    }

    async fn modify_video(
        &self,
        id: u16,
        watermark: Option<bool>,
        osd: Option<bool>,
    ) -> Result<(), CamAvError> {
        info!(id, ?watermark, ?osd, "Matter camera: modify video stream");
        Ok(())
    }

    async fn deallocate_video(&self, id: u16) -> Result<(), CamAvError> {
        info!(id, "Matter camera: deallocate video stream");
        Ok(())
    }
}

/// Zone Management hooks — read-only reflection of `[[ai.rules]]`. This
/// firmware's zones are config-owned; a controller can see them (they're
/// pre-seeded as manufacturer/`Mfg` zones at boot via `add_mfg_zone`) but
/// cannot create, modify, or remove them — those requests are rejected
/// rather than silently accepted and then ignored. Trigger events
/// (dwell-time arm/disarm windows) are logged only for now; wiring them
/// to actually gate `ai::rules::RuleEngine` zones live is a real,
/// separate follow-up (see TODO.md), not attempted in this pass.
pub(crate) struct ReadOnlyZoneHooks;

impl ZoneMgmtHooks<ZONE_NV> for ReadOnlyZoneHooks {
    async fn zone_created(&self, zone: &Zone<ZONE_NV>) -> Result<(), ZoneError> {
        // Only reachable if `USER_DEFINED` were advertised, which it
        // isn't (see `ZONE_CLUSTER` below) — a controller's
        // CreateTwoDCartesianZone is rejected by the handler itself
        // before this hook ever runs. Logged defensively in case that
        // assumption ever changes.
        warn!(zone_id = zone.zone_id, "Matter camera: rejecting controller-created zone (zones are config-owned by [[ai.rules]])");
        Err(ZoneError::Failure)
    }

    async fn zone_removed(&self, id: u16) -> Result<(), ZoneError> {
        warn!(zone_id = id, "Matter camera: rejecting controller zone removal (zones are config-owned by [[ai.rules]])");
        Err(ZoneError::Failure)
    }

    async fn trigger_set(&self, t: &Trigger) -> Result<(), ZoneError> {
        debug!(
            zone_id = t.zone_id,
            initial_s = t.initial_duration,
            max_s = t.max_duration,
            "Matter camera: zone trigger window set (not yet wired to ai::rules::RuleEngine)"
        );
        Ok(())
    }

    /// `Sensitivity` reflects the configured detector (see `zone_sensitivity`) and
    /// is read-only: changing the AI confidence threshold at runtime isn't
    /// supported, so a write is refused instead of being accepted and ignored.
    async fn set_sensitivity(&self, value: u8) -> Result<(), ZoneError> {
        warn!(value, "Matter camera: rejecting Sensitivity write (it reflects [ai].confidence_threshold, which is config-owned)");
        Err(ZoneError::Failure)
    }
}

/// The camera's ZoneManagement metadata: ONLY what a camera with READ-ONLY zones
/// (config-owned by `[[ai.rules]]`) implements. rs-matter's stock
/// `ZoneMgmt::CLUSTER` also claims the USER_DEFINED feature — zones a controller
/// can create, update and remove, which would require `MaxUserDefinedZones >= 5` and
/// the Create/Update/RemoveZone commands — so a conformance check read
/// `MaxUserDefinedZones = 0` (below the spec minimum) off a camera that rejects every
/// such request. Advertised here: the 2-D Cartesian zone feature, the five
/// attributes it needs (SensitivityMax and Sensitivity are mandatory without
/// per-zone sensitivity), and the two trigger commands that are mandatory.
pub(crate) const ZONE_CLUSTER: Cluster<'static> = ZoneMgmt::CLUSTER
    .with_features(ZoneFeature::TWO_DIMENSIONAL_CARTESIAN_ZONE.bits())
    .with_attrs(with!(
        required;
        ZoneAttr::MaxZones
            | ZoneAttr::Zones
            | ZoneAttr::Triggers
            | ZoneAttr::SensitivityMax
            | ZoneAttr::Sensitivity
            | ZoneAttr::TwoDCartesianMax
    ))
    .with_cmds(with!(
        ZoneCmd::CreateOrUpdateTrigger | ZoneCmd::RemoveTrigger
    ));

/// `SensitivityMax`: the spec's maximum (the attribute must be 2..=10).
const ZONE_SENSITIVITY_MAX: u8 = 10;

/// The zone `Sensitivity` (1 = least .. 10 = most sensitive) as the configured
/// detector implies it: the AI confidence threshold is the sensitivity knob zone
/// detection (`[[ai.rules]]`) runs on — a LOWER threshold fires on weaker evidence
/// — so 0.60 reads as 4, 0.50 as 5, 0.90 as 1. A real figure for the detector as
/// configured, not an invented default.
fn zone_sensitivity(ai_confidence_threshold: f32) -> u8 {
    if !ai_confidence_threshold.is_finite() {
        return 5;
    }
    ((1.0 - ai_confidence_threshold.clamp(0.0, 1.0)) * f32::from(ZONE_SENSITIVITY_MAX))
        .round()
        .clamp(1.0, f32::from(ZONE_SENSITIVITY_MAX)) as u8
}

/// Converts `[[ai.rules]]` zones (normalized `[0,1]` polygon points) into
/// Matter `Zone` structs (pixel coordinates in the camera's real
/// resolution, matching `ZoneMgmtConfig.two_d_cartesian_max`). Only
/// `presence`/`loiter` rules (3+ point polygons) map onto Matter's 2-D
/// Cartesian zone concept; `line_cross` rules (2-point lines) have no
/// Matter Zone Management equivalent and are skipped, not force-fit.
fn ai_rules_to_matter_zones(rules: &[AiRule], camera: &CameraConfig) -> Vec<Zone<ZONE_NV>> {
    let (w, h) = (camera.width as f32, camera.height as f32);
    let mut next_id = 1u16;
    rules
        .iter()
        .filter(|r| r.enabled && r.zone.len() >= 3 && r.zone.len() <= ZONE_NV)
        .filter_map(|rule| {
            let mut vertices = HVec::new();
            for [x, y] in &rule.zone {
                let px = (x.clamp(0.0, 1.0) * w) as u16;
                let py = (y.clamp(0.0, 1.0) * h) as u16;
                if vertices.push((px, py)).is_err() {
                    return None; // exceeded ZONE_NV — skip this rule entirely rather than truncate its shape
                }
            }
            // `push_str` fails (rather than truncating) if `rule.name`
            // doesn't fit in Matter's 16-byte zone-name limit — skip the
            // rule rather than risk a byte-boundary panic from manually
            // slicing a UTF-8 string, or silently handing the controller
            // a truncated/duplicate-looking name.
            let mut name: heapless::String<
                { rs_matter::dm::clusters::app::zone_mgmt::MAX_ZONE_NAME_LEN },
            > = heapless::String::new();
            name.push_str(&rule.name).ok()?;
            let zone_id = next_id;
            next_id += 1;
            Some(Zone {
                zone_id,
                zone_type: ZoneTypeEnum::TwoDCARTZone,
                zone_source: ZoneSourceEnum::Mfg,
                name,
                zone_use: ZoneUseEnum::Motion,
                vertices,
                color: None,
            })
        })
        .collect()
}

// --- The rest of this module (WebRTC/str0m bridging) is a close
// adaptation of rs-matter's own webrtc_camera example — see this file's
// header comment for what changed and why. ---

struct SessionCtrl {
    remote_cand_tx: Sender<Candidate>,
    shutdown_tx: Sender<()>,
    trickle_buf: Rc<RefCell<Vec<String>>>,
    answer_sdp: RefCell<Option<String>>,
}

struct NewSession {
    id: u16,
    rtc: Rtc,
    socket: Async<UdpSocket>,
    local_addr: SocketAddr,
    remote_cand_rx: Receiver<Candidate>,
    shutdown_rx: Receiver<()>,
    outbound_tx: Sender<OutboundWork>,
    live_frames: std_mpsc::Receiver<H264Frame>,
}

struct Str0mInner {
    sessions: Vec<(u16, SessionCtrl)>,
}

impl Str0mInner {
    fn get(&self, id: u16) -> Option<&SessionCtrl> {
        self.sessions.iter().find(|(k, _)| *k == id).map(|(_, v)| v)
    }
    fn remove(&mut self, id: u16) -> Option<SessionCtrl> {
        let pos = self.sessions.iter().position(|(k, _)| *k == id)?;
        Some(self.sessions.swap_remove(pos).1)
    }
    fn insert(&mut self, id: u16, ctrl: SessionCtrl) {
        self.sessions.retain(|(k, _)| *k != id);
        self.sessions.push((id, ctrl));
    }
    fn len(&self) -> usize {
        self.sessions.len()
    }
}

struct Str0mShared {
    inner: RefCell<Str0mInner>,
    new_session_tx: Sender<NewSession>,
    new_session_rx: Receiver<NewSession>,
    outbound_tx: Sender<OutboundWork>,
    outbound_rx: Receiver<OutboundWork>,
    live_source: Arc<LiveH264Source>,
}

impl Str0mShared {
    fn new(live_source: Arc<LiveH264Source>) -> Self {
        let (new_session_tx, new_session_rx) = async_channel::unbounded();
        let (outbound_tx, outbound_rx) = async_channel::unbounded();
        Self {
            inner: RefCell::new(Str0mInner {
                sessions: Vec::new(),
            }),
            new_session_tx,
            new_session_rx,
            outbound_tx,
            outbound_rx,
            live_source,
        }
    }

    async fn drive(&'static self) -> Result<(), rs_matter::error::Error> {
        let ex: LocalExecutor<'static> = LocalExecutor::new();
        let accept = async {
            while let Ok(new) = self.new_session_rx.recv().await {
                let sid = new.id;
                info!(
                    session = sid,
                    "Matter camera: spawning WebRTC session driver"
                );
                ex.spawn(session_loop(new)).detach();
            }
        };
        ex.run(accept).await;
        Ok(())
    }
}

#[derive(Copy, Clone)]
pub(crate) struct Str0mHooks {
    shared: &'static Str0mShared,
}

fn packet_kind(_data: &[u8]) -> &'static str {
    "udp"
}

/// Drives one `Rtc` + its UDP socket to completion. Closely follows the
/// reference example's `session_loop` (ICE/DTLS state machine, poll_output/
/// handle_input pumping) with one deliberate difference: media pacing.
/// The reference replays a static, preloaded frame array on a fixed-fps
/// timer (`next_frame_at = at + frame_interval`); this reads whatever the
/// live encoder (src/matter/encoder.rs) actually produced since the last
/// iteration and stamps the RTP clock off real elapsed wall time between
/// frames, so pacing tracks the encoder's true cadence instead of a
/// simulated one.
///
/// Also simplified vs. the reference: candidate/shutdown signals are
/// polled non-blockingly at the top of every loop iteration rather than
/// raced 4-way against the socket recv and str0m's own deadline. This
/// adds at most ~100ms of signaling latency (the recv wait is capped at
/// 100ms below) in exchange for a much smaller state machine — acceptable
/// for v1; tighten if a real controller round-trip proves this matters.
async fn session_loop(session: NewSession) {
    let NewSession {
        id,
        mut rtc,
        socket,
        local_addr,
        remote_cand_rx,
        shutdown_rx,
        outbound_tx,
        live_frames,
    } = session;

    let mut buf = vec![0u8; 2048];
    let mut end_reason = WebRTCEndReasonEnum::ICEFailed;
    let mut video_mid: Option<Mid> = None;
    let mut video_pt: Option<Pt> = None;
    let mut connected = false;
    // RTP clock runs off wall time between successive real frames rather
    // than a simulated fixed-fps step, since frames arrive whenever the
    // encoder actually produces one.
    let mut last_frame_at: Option<Instant> = None;
    let mut rtp_ts: u64 = 0;

    'outer: loop {
        while let Ok(c) = remote_cand_rx.try_recv() {
            rtc.add_remote_candidate(c);
        }
        if shutdown_rx.try_recv().is_ok() {
            end_reason = WebRTCEndReasonEnum::UserHangup;
            break 'outer;
        }

        // IMPORTANT: after `writer.write()` we must let the loop fall
        // through to `poll_output` before writing again — str0m requires
        // a `poll_output` call between consecutive writes. No `continue`
        // here guarantees that even when a burst of frames is pending.
        if connected {
            if let (Some(mid), Some(pt)) = (video_mid, video_pt) {
                if let Ok(frame) = live_frames.try_recv() {
                    let now = Instant::now();
                    let elapsed = last_frame_at
                        .map(|t| now.duration_since(t))
                        .unwrap_or(Duration::from_millis(33));
                    last_frame_at = Some(now);
                    // 90 kHz video clock, matching str0m's MediaTime convention.
                    rtp_ts = rtp_ts.wrapping_add((elapsed.as_secs_f64() * 90_000.0) as u64);
                    if let Some(writer) = rtc.writer(mid) {
                        let mtime = MediaTime::from_90khz(rtp_ts);
                        if let Err(e) = writer.write(pt, now, mtime, frame.data.to_vec()) {
                            warn!(session = id, "Matter camera: WebRTC write failed: {e}");
                        }
                    }
                }
            }
        }

        let out = match rtc.poll_output() {
            Ok(o) => o,
            Err(e) => {
                warn!(session = id, "Matter camera: poll_output failed: {e}");
                break 'outer;
            }
        };

        match out {
            Output::Transmit(t) => {
                let _ = packet_kind(&t.contents);
                if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                    warn!(
                        session = id,
                        "Matter camera: UDP send_to {} failed: {e}", t.destination
                    );
                }
                continue;
            }
            Output::Timeout(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    if let Err(e) = rtc.handle_input(Input::Timeout(now)) {
                        warn!(
                            session = id,
                            "Matter camera: handle_input(Timeout) failed: {e}"
                        );
                        break 'outer;
                    }
                    continue;
                }
                let timeout = deadline.saturating_duration_since(now);
                let recv_fut = socket.recv_from(&mut buf);
                let timer_fut = async_io::Timer::after(timeout.min(Duration::from_millis(100)));
                // Bound to an owned value in its own statement (not directly
                // in the `if let` scrutinee) so the pinned futures' `&mut buf`
                // borrow ends here — Rust extends a scrutinee temporary's
                // lifetime across the whole `if let`, which would otherwise
                // keep that borrow alive into the body's `&buf[..n]` below.
                let outcome = select(pin!(recv_fut), pin!(timer_fut)).await;
                if let embassy_futures::select::Either::First(Ok((n, source))) = outcome {
                    match Receive::new(Protocol::Udp, source, local_addr, &buf[..n]) {
                        Ok(r) => {
                            if let Err(e) = rtc.handle_input(Input::Receive(Instant::now(), r)) {
                                warn!(
                                    session = id,
                                    "Matter camera: handle_input(Receive) failed: {e}"
                                );
                            }
                        }
                        Err(_) => { /* non-WebRTC packet on this socket — ignore */ }
                    }
                }
            }
            Output::Event(Event::Connected) => {
                connected = true;
                info!(session = id, "Matter camera: ICE+DTLS connected");
                if video_pt.is_some() {
                    last_frame_at = None;
                }
            }
            Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => {
                end_reason = WebRTCEndReasonEnum::ICEFailed;
                break 'outer;
            }
            Output::Event(Event::MediaAdded(m)) => {
                info!(session = id, mid = ?m.mid, kind = ?m.kind, "Matter camera: MediaAdded");
                if m.kind == MediaKind::Video && video_mid.is_none() {
                    if let Some(writer) = rtc.writer(m.mid) {
                        let pt = writer
                            .payload_params()
                            .find(|p| p.spec().codec == RtcCodec::H264)
                            .map(|p| p.pt());
                        if let Some(pt) = pt {
                            video_mid = Some(m.mid);
                            video_pt = Some(pt);
                            info!(session = id, ?pt, "Matter camera: H264 payload type bound");
                        } else {
                            warn!(session = id, "Matter camera: no H264 payload type negotiated for this media line");
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let _ = outbound_tx
        .send(OutboundWork::End {
            session_id: id,
            reason: end_reason,
        })
        .await;
}

impl WebRtcHooks for Str0mHooks {
    async fn on_solicit_offer(
        &self,
        session_id: u16,
        _params: &OfferParams,
    ) -> Result<SolicitOutcome, WebRtcError> {
        warn!(
            session = session_id,
            "Matter camera: camera-initiated offer flow not implemented"
        );
        Err(WebRtcError::Failure)
    }

    async fn on_offer(
        &self,
        session_id: u16,
        sdp: &str,
        _params: &OfferParams,
    ) -> Result<AnswerOutcome, WebRtcError> {
        info!(
            session = session_id,
            offer_len = sdp.len(),
            "Matter camera: WebRTC offer received"
        );

        let offer = SdpOffer::from_sdp_string(sdp).map_err(|_| WebRtcError::DynamicConstraint)?;

        let bind_addr: SocketAddr = ([0u8, 0, 0, 0], 0u16).into();
        let socket = Async::<UdpSocket>::bind(bind_addr).map_err(|_| WebRtcError::Failure)?;
        let local_addr = socket
            .as_ref()
            .local_addr()
            .map_err(|_| WebRtcError::Failure)?;

        let host_ip = UdpSocket::bind(SocketAddr::from(([0u8, 0, 0, 0], 0u16)))
            .and_then(|s| {
                s.connect(SocketAddr::from(([198u8, 51, 100, 1], 80)))?;
                s.local_addr()
            })
            .ok()
            .map(|a| a.ip())
            .filter(|ip| !ip.is_unspecified())
            .ok_or(WebRtcError::Failure)?;
        let host_addr = SocketAddr::new(host_ip, local_addr.port());

        let mut rtc = Rtc::new(Instant::now());
        let cand = Candidate::host(host_addr, "udp").map_err(|_| WebRtcError::Failure)?;
        let local_cand_sdp = cand.to_sdp_string();
        rtc.add_local_candidate(cand);

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|_| WebRtcError::DynamicConstraint)?;
        let answer_sdp = answer.to_sdp_string();

        let (remote_cand_tx, remote_cand_rx) = async_channel::unbounded();
        let (shutdown_tx, shutdown_rx) = async_channel::bounded(1);
        let trickle_buf = Rc::new(RefCell::new(vec![local_cand_sdp]));

        self.shared.inner.borrow_mut().insert(
            session_id,
            SessionCtrl {
                remote_cand_tx,
                shutdown_tx,
                trickle_buf: trickle_buf.clone(),
                answer_sdp: RefCell::new(Some(answer_sdp)),
            },
        );

        let live_frames = self.shared.live_source.subscribe();
        self.shared
            .new_session_tx
            .send(NewSession {
                id: session_id,
                rtc,
                socket,
                local_addr: host_addr,
                remote_cand_rx,
                shutdown_rx,
                outbound_tx: self.shared.outbound_tx.clone(),
                live_frames,
            })
            .await
            .map_err(|_| WebRtcError::Failure)?;

        let _ = self
            .shared
            .outbound_tx
            .send(OutboundWork::Answer { session_id })
            .await;
        let _ = self
            .shared
            .outbound_tx
            .send(OutboundWork::IceCandidates { session_id })
            .await;

        Ok(AnswerOutcome {
            video_stream_id: None,
            audio_stream_id: None,
        })
    }

    async fn on_answer(&self, session_id: u16, _sdp: &str) -> Result<(), WebRtcError> {
        warn!(
            session = session_id,
            "Matter camera: unexpected ProvideAnswer"
        );
        Err(WebRtcError::InvalidInState)
    }

    async fn on_ice_candidates(
        &self,
        session_id: u16,
        candidates: &TLVArray<'_, ICECandidateStruct<'_>>,
    ) -> Result<(), WebRtcError> {
        let mut parsed = Vec::new();
        for cand in candidates.iter().flatten() {
            if let Ok(sdp_line) = cand.candidate() {
                if let Ok(c) = Candidate::from_sdp_string(sdp_line.trim_start_matches("a=")) {
                    parsed.push(c);
                }
            }
        }
        let sender = {
            let inner = self.shared.inner.borrow();
            let Some(s) = inner.get(session_id) else {
                return Err(WebRtcError::InvalidInState);
            };
            s.remote_cand_tx.clone()
        };
        for c in parsed {
            let _ = sender.send(c).await;
        }
        Ok(())
    }

    async fn on_end_session(
        &self,
        session_id: u16,
        reason: WebRTCEndReasonEnum,
    ) -> Result<(), WebRtcError> {
        if let Some(ctrl) = self.shared.inner.borrow_mut().remove(session_id) {
            let _ = ctrl.shutdown_tx.try_send(());
            info!(
                session = session_id,
                ?reason,
                active = self.shared.inner.borrow().len(),
                "Matter camera: WebRTC session ended"
            );
        }
        Ok(())
    }

    async fn next_outbound(&self) -> OutboundWork {
        // If the sender half is ever dropped, park forever — same
        // contract as the trait's own default implementation — rather
        // than fabricate a work item that doesn't exist.
        match self.shared.outbound_rx.recv().await {
            Ok(w) => w,
            Err(_) => core::future::pending().await,
        }
    }

    async fn take_answer_sdp(
        &self,
        session_id: u16,
        sdp_out: &mut [u8],
    ) -> Result<usize, WebRtcError> {
        // Plain `match` rather than `let-else` here: the bound reference
        // (`s`, borrowed from the `RefCell` guard `inner`) needs to stay
        // inside the guard's own scope, which is cleanest to guarantee
        // with the borrow-and-use kept in one match arm.
        let taken = {
            let inner = self.shared.inner.borrow();
            match inner.get(session_id) {
                Some(s) => s.answer_sdp.borrow_mut().take(),
                None => {
                    warn!(
                        session_id,
                        "Matter camera: take_answer_sdp for unknown session"
                    );
                    return Err(WebRtcError::InvalidInState);
                }
            }
        };
        let sdp = taken.ok_or_else(|| {
            warn!(
                session_id,
                "Matter camera: take_answer_sdp: no Answer queued"
            );
            WebRtcError::InvalidInState
        })?;
        if sdp.len() > sdp_out.len() {
            warn!(
                session_id,
                sdp_len = sdp.len(),
                buf_len = sdp_out.len(),
                "Matter camera: answer SDP exceeds buffer"
            );
            return Err(WebRtcError::ResourceExhausted);
        }
        sdp_out[..sdp.len()].copy_from_slice(sdp.as_bytes());
        Ok(sdp.len())
    }

    async fn take_ice_candidates(
        &self,
        session_id: u16,
        out: &mut dyn rs_matter::dm::clusters::app::webrtc_prov::IceCandidateSink,
    ) -> Result<(), WebRtcError> {
        // Snapshot-and-drain: the handler iterates `out` inside a sync
        // build closure that MRP may re-run on retransmit, but that
        // closure works off the snapshot it already wrote here (not this
        // queue), so re-runs are idempotent even though this drains once.
        let inner = self.shared.inner.borrow();
        let Some(s) = inner.get(session_id) else {
            return Ok(());
        };
        let drained: Vec<String> = s.trickle_buf.borrow_mut().drain(..).collect();
        for cand in &drained {
            out.push(cand)?;
        }
        Ok(())
    }
}

/// Everything this module owns, built once at startup and handed to
/// `matter::mod::spawn` for the `NODE`/`data_model()` wiring. Ownership
/// stays here rather than in `mod.rs` so the WebRTC/zone/stream internals
/// (all `'static`-borrowed by rs-matter's handler adaptors) are a single
/// cohesive unit.
pub struct MatterCamera {
    // crate-visible, not `pub`: `WebRtc`/`CamAv`/`ZoneMgmt` are each
    // parameterized by a private hook type (`Str0mHooks`/`CamHooks`/
    // `ReadOnlyZoneHooks`), so a `pub` field here would be more exposed
    // than its own type is nameable from outside the crate.
    pub(crate) webrtc: &'static WebRtc,
    pub(crate) cam_av: &'static CamAv,
    pub(crate) zone_mgmt: &'static ZoneMgmt,
    driver: &'static Str0mShared,
}

impl MatterCamera {
    pub fn new(
        rand: &mut impl rand_core::Rng,
        camera_cfg: &CameraConfig,
        stream_cfg: &StreamConfig,
        ai_rules: &[AiRule],
        ai_confidence_threshold: f32,
        live_source: Arc<LiveH264Source>,
    ) -> &'static Self {
        use rs_matter::dm::Dataver;

        let driver: &'static Str0mShared = Box::leak(Box::new(Str0mShared::new(live_source)));

        let webrtc: &'static WebRtc = Box::leak(Box::new(WebRtcProvHandler::new(
            Dataver::new_rand(rand),
            1,
            Str0mHooks { shared: driver },
        )));

        let stream_usages: &'static [StreamUsageEnum] = &[StreamUsageEnum::LiveView];
        let rate_points: &'static [RateDistortionPoint] =
            Box::leak(Box::new([RateDistortionPoint {
                codec: VideoCodecEnum::H264,
                min_resolution: (640, 360),
                min_bit_rate: (stream_cfg.bitrate_kbps.min(500)) * 1000,
            }]));
        let cam_av_config = CameraAvStreamConfig {
            max_concurrent_encoders: 1,
            max_encoded_pixel_rate: camera_cfg.width * camera_cfg.height * camera_cfg.fps,
            sensor: VideoSensorParams {
                sensor_width: camera_cfg.width as u16,
                sensor_height: camera_cfg.height as u16,
                max_fps: camera_cfg.fps as u16,
                max_hdrfps: None,
            },
            min_viewport: (640, 360),
            max_content_buffer_size: 1_048_576,
            max_network_bandwidth: stream_cfg.bitrate_kbps,
            supported_stream_usages: stream_usages,
            default_stream_usage_priorities: stream_usages,
            rate_distortion_points: rate_points,
            mic_capabilities: None,
        };
        let cam_av: &'static CamAv = Box::leak(Box::new(CameraAvStreamHandler::new(
            Dataver::new_rand(rand),
            1,
            cam_av_config,
            CamAvFeature::VIDEO.bits(),
            CamHooks,
        )));
        let _ = cam_av.add_preallocated_video(VideoStream {
            video_stream_id: 0,
            stream_usage: StreamUsageEnum::LiveView,
            video_codec: VideoCodecEnum::H264,
            min_frame_rate: 1,
            max_frame_rate: camera_cfg.fps as u16,
            min_width: 640,
            min_height: 360,
            max_width: camera_cfg.width as u16,
            max_height: camera_cfg.height as u16,
            min_bit_rate: 500_000,
            max_bit_rate: (stream_cfg.bitrate_kbps) * 1000,
            key_frame_interval: (stream_cfg.gop_size * 1000 / camera_cfg.fps.max(1)) as u16,
            watermark_enabled: None,
            osd_enabled: None,
            reference_count: 0,
        });

        let zone_mgmt: &'static ZoneMgmt = Box::leak(Box::new(ZoneMgmtHandler::new(
            Dataver::new_rand(rand),
            1,
            ZoneMgmtConfig {
                max_zones: ZONE_NZ as u8,
                max_user_defined_zones: 0, // read-only reflection — see ZONE_CLUSTER
                sensitivity_max: ZONE_SENSITIVITY_MAX,
                default_sensitivity: zone_sensitivity(ai_confidence_threshold),
                two_d_cartesian_max: (camera_cfg.width as u16, camera_cfg.height as u16),
            },
            ZoneFeature::TWO_DIMENSIONAL_CARTESIAN_ZONE.bits(),
            ReadOnlyZoneHooks,
        )));
        for zone in ai_rules_to_matter_zones(ai_rules, camera_cfg) {
            if let Err(e) = zone_mgmt.add_mfg_zone(zone) {
                warn!("Matter camera: failed to pre-seed an ai.rules zone: {e}");
            }
        }

        Box::leak(Box::new(Self {
            webrtc,
            cam_av,
            zone_mgmt,
            driver,
        }))
    }

    /// The async driver future — join this into the top-level `select` in
    /// `matter::mod::run` alongside transport/mdns/responder/im_job.
    pub async fn drive(&'static self) -> Result<(), rs_matter::error::Error> {
        self.driver.drive().await
    }
}

/// The Camera device as a registry endpoint (fixed endpoint id 1 — the same id
/// every previously-paired controller already knows it by). The Descriptor
/// cluster is added by the registry; the three camera clusters are listed
/// here in the order they have always been advertised.
///
/// No Groups cluster: it needs the `"groups"` Cargo feature, which also
/// activates rs-matter's Groupcast multicast transport — confirmed to fail
/// with `StdIoError` on the macOS/BSD IPv6 stack (interface-index-0 multicast
/// join rejected, unlike Linux) — and a Camera doesn't need scene/group
/// control. See Cargo.toml's comment on the `rs-matter` dependency.
pub(crate) fn spec(cam: &'static MatterCamera) -> EndpointSpec {
    EndpointSpec {
        id: CAMERA_ENDPOINT_ID,
        dynamic: false,
        name: "camera".to_string(),
        device_types: vec![DEV_TYPE_MATTER_CAMERA],
        clusters: vec![
            (
                CamAv::CLUSTER,
                ClusterImpl::CamAv(
                    rs_matter::dm::clusters::app::cam_av_stream::HandlerAsyncAdaptor(cam.cam_av),
                ),
            ),
            (
                ZONE_CLUSTER,
                ClusterImpl::ZoneMgmt(ZoneMgmtAdaptor(cam.zone_mgmt)),
            ),
            (
                WebRtc::CLUSTER,
                ClusterImpl::WebRtc(WebRtcAdaptor(cam.webrtc)),
            ),
        ],
    }
}

/// Referenced by PTZ follow-up work (see this file's header) — kept as a
/// documented non-call so the intended integration seam is visible in
/// the code, not just in a comment.
#[allow(dead_code)]
fn _future_ptz_seam(_ptz: &PtzController) {}

#[cfg(test)]
mod zone_tests {
    use super::*;

    #[test]
    fn sensitivity_follows_the_configured_confidence_threshold() {
        assert_eq!(zone_sensitivity(0.60), 4, "the shipped default threshold");
        assert_eq!(zone_sensitivity(0.50), 5);
        assert_eq!(zone_sensitivity(0.90), 1);
        assert_eq!(
            zone_sensitivity(0.0),
            10,
            "fires on the weakest evidence = most sensitive"
        );
        assert_eq!(
            zone_sensitivity(1.0),
            1,
            "never below the spec minimum of 1"
        );
        // Nonsense in config must not produce a nonsense attribute.
        assert_eq!(zone_sensitivity(f32::NAN), 5);
        assert_eq!(zone_sensitivity(2.5), 1);
        assert_eq!(zone_sensitivity(-1.0), 10);
        for t in 0..=100 {
            let s = zone_sensitivity(t as f32 / 100.0);
            assert!(
                (1..=ZONE_SENSITIVITY_MAX).contains(&s),
                "threshold {t}% -> {s}"
            );
        }
    }

    #[test]
    fn the_spec_range_for_sensitivity_max_is_respected() {
        assert!((2..=10).contains(&ZONE_SENSITIVITY_MAX));
    }

    #[test]
    fn the_zone_cluster_advertises_only_what_a_read_only_camera_implements() {
        let c = ZONE_CLUSTER;
        assert_eq!(
            c.feature_map,
            ZoneFeature::TWO_DIMENSIONAL_CARTESIAN_ZONE.bits(),
            "no USER_DEFINED"
        );
        for a in [
            ZoneAttr::MaxZones,
            ZoneAttr::Zones,
            ZoneAttr::Triggers,
            ZoneAttr::SensitivityMax,
            ZoneAttr::Sensitivity,
            ZoneAttr::TwoDCartesianMax,
        ] {
            assert!(c.attribute(a as _).is_some(), "{a:?} must be advertised");
        }
        assert!(
            c.attribute(ZoneAttr::MaxUserDefinedZones as _).is_none(),
            "MaxUserDefinedZones belongs to USER_DEFINED (and must then be >= 5)"
        );
        for cmd in [ZoneCmd::CreateOrUpdateTrigger, ZoneCmd::RemoveTrigger] {
            assert!(c.command(cmd as _).is_some(), "{cmd:?}");
        }
        for cmd in [
            ZoneCmd::CreateTwoDCartesianZone,
            ZoneCmd::UpdateTwoDCartesianZone,
            ZoneCmd::RemoveZone,
        ] {
            assert!(
                c.command(cmd as _).is_none(),
                "{cmd:?} is USER_DEFINED-only"
            );
        }
    }
}
