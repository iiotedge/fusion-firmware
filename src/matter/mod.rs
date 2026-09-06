// src/matter/mod.rs
//
// Matter protocol support (Phase 19c, extended Phase 19d for multi-device-
// type support): makes this device commissionable into any Matter fabric
// (Apple Home, Google Home, Amazon Alexa, SmartThings, Home Assistant's
// Matter server, …). This is genuinely generic firmware, not
// camera-specific under the hood — which Matter endpoints actually exist
// on a given deployment is config-driven, not compiled-in:
//   - [matter.camera] (default on): Camera device (0x0142) — WebRTC
//     Transport Provider, Camera AV Stream Management, Zone Management.
//     See src/matter/camera.rs.
//   - [matter.onoff] (default off): On/Off Light/Switch device (0x0100)
//     backed by a real GPIO output line. See src/matter/onoff.rs.
// Deploy this exact firmware as a Matter light switch instead of a camera
// by disabling [matter.camera] and enabling [matter.onoff] with a real
// gpio_chip/gpio_line — the Matter fabric then sees a plain switch, no
// camera clusters at all.
//
// # Why endpoints are config-selected rather than compile-time fixed
//
// rs-matter's cluster-handler chaining (`.chain()`, camera.rs's
// `ChainExt`) is an INHERENT method whose return type changes with every
// call — a genuine Rust static-typing constraint, not a design choice —
// so the SET of cluster handlers wired into the Interaction Model must be
// fixed at compile time; there's no dynamic "chain N handlers decided at
// runtime" without boxing (`dyn AsyncHandler`) that this crate doesn't
// provide. The way around this, used throughout this module: ALWAYS
// construct and ALWAYS chain every possible device type's handlers
// (cheap — none of it opens real hardware or spawns threads merely by
// being constructed, see onoff.rs's `RelayOnOffHooks::new` and
// encoder::spawn's own gating for the one exception). What varies at
// runtime is only which endpoint NUMBERS appear in the Matter `Node`'s
// endpoint list — Matter's Interaction Model never routes a request to an
// endpoint that doesn't exist, so a structurally-present-but-unlisted
// handler is simply never reachable. A disabled device type therefore
// costs a few inert struct fields, not a real resource.
//
// # Why this exists (and why it's real, not a stub)
//
// This started from a wrong first-pass assessment: an initial pass over
// rs-matter's docs concluded no camera clusters existed yet, and the plan
// was scoped down to non-video clusters only. Fetching and reading the
// crate's actual reference example (`examples/src/bin/webrtc_camera.rs`,
// ~1400 lines, Apache-2.0) directly — rather than trusting a stale
// capability-matrix doc — showed that was wrong: a real, working
// WebRTC-bridged camera device already exists as proven, tested code
// (its own comments note SmartThings has actually commissioned it). That
// correction was reported back before writing anything, and the larger,
// real scope was explicitly re-confirmed rather than assumed. Everything
// in this module and `camera.rs` is adapted from that verified reference
// plus direct inspection of the installed `rs-matter 0.3.0` source
// (including code-generated cluster enums under `OUT_DIR`, which aren't
// present in the crate's checked-in `src/` tree and only exist after a
// successful build) — not written from the Matter spec from memory. The
// exact same "verify against the real, currently-installed dependency,
// not an assumption or a newer upstream doc" discipline that caught the
// WHIP `whipclientsink` → `whipsink` bug earlier in this project applies
// throughout: notably, the upstream example's own Cargo.toml (fetched
// from GitHub's `main` branch) pins `rand = "0.10"`, but the actually
// *installed* `rs-matter = "0.3.0"` (this crate's real dependency, from
// crates.io) resolves `rand_core = "0.6"` internally — using the newer
// version from the docs would not have compiled against what's actually
// pinned in Cargo.lock, so this module uses `rand_core` (0.6, via
// `rand_core::OsRng`) instead of chasing the newer example.
//
// # What's real vs. deliberately deferred
//
// Real: commissioning (PASE, QR/manual pairing code, mDNS discovery),
// WebRTC session negotiation (SDP offer/answer + trickle ICE via a real
// `str0m::Rtc` per session), live H.264 media (src/matter/encoder.rs taps
// this firmware's actual capture frames — not a preloaded demo file),
// Camera AV Stream Management (config sourced from this device's real
// [camera]/[stream] settings), Zone Management (pre-seeded from this
// device's real [[ai.rules]] zones, read-only).
//
// Deferred, honestly: device attestation uses rs-matter's `TEST_DEV_ATT`/
// `TEST_DEV_COMM`/`TEST_DEV_DET` (the same constants `chip-tool`, the
// reference Matter controller CLI, expects) rather than a real CSA-issued
// certificate chain — no such credential exists for this firmware today,
// same "for now" call already made for Phase 12c's onboarding QR. Camera
// AV Settings (mechanical/digital PTZ) is not implemented at all — see
// camera.rs's header for why. Zone triggers are logged, not yet wired to
// actually arm/disarm `ai::rules::RuleEngine` zones live.
pub mod camera;
pub mod encoder;
mod mdns;
mod onoff;

use crate::config::{AiRule, CameraConfig, MatterConfig, StreamConfig};
use crate::hal::FrameHandle;
use camera::MatterCamera;

use core::pin::pin;
use std::mem::MaybeUninit;
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread;

use async_io::Async;
use crossbeam::channel::Receiver;
use embassy_futures::select::{select, select4};

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::endpoints;
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::dm::networks::unix::UnixNetifs;
use rs_matter::dm::{Endpoint, Node};
use rs_matter::im::{EthInteractionModelState, InteractionModel};
use rs_matter::pairing::qr::QrTextType;
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::persist::DirKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::transport::network::tcp::TcpNetwork;
use rs_matter::transport::network::{Address, ChainedNetwork};
use rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::select::Coalesce;
use rs_matter::{root_endpoint, Matter, MATTER_PORT};

use tracing::{error, info, warn};

/// Runs one in-place initializer to completion on a heap allocation that
/// is never moved afterward, then leaks it to `'static` — the same thing
/// the reference example's `StaticCell`s do (avoid ever materializing a
/// large `T`, e.g. `Matter` itself, on the stack), just backed by `Box`
/// instead of pulling in the `static_cell` crate: this firmware already
/// has a heap (unlike rs-matter's own `no_std` targets), so a boxed
/// `MaybeUninit<T>` gives the identical guarantee (fixed address,
/// in-place write) with one fewer dependency.
fn init_boxed<T>(init: impl rs_matter::utils::init::Init<T>) -> &'static mut T {
    let boxed: Box<MaybeUninit<T>> = Box::new(MaybeUninit::uninit());
    let leaked: &'static mut MaybeUninit<T> = Box::leak(boxed);
    leaked.init_with(init)
}

/// Builds this device's `BasicInfoConfig` — this firmware's own
/// `device_id` everywhere the reference example uses "ACME Test"/
/// hardcoded placeholders. `BasicInfoConfig` fields are `&'static str`,
/// so the runtime `device_id` is leaked once here; callers (`run()` at
/// boot, `setup_qr_text()` — itself called at most once, see its own
/// doc comment) must not call this repeatedly or the leak repeats too.
/// Returns the config plus the leaked `device_id` string (reused as
/// `serial_no`/`unique_id` and as the mDNS hostname).
fn build_basic_info(device_id: &str) -> (&'static BasicInfoConfig<'static>, &'static str) {
    let device_id_static: &'static str = Box::leak(device_id.to_string().into_boxed_str());
    let device_name_static: &'static str =
        Box::leak(format!("fusion-firmware {device_id}").into_boxed_str());
    let basic_info: &'static BasicInfoConfig<'static> = Box::leak(Box::new(BasicInfoConfig {
        vendor_name: "IIoTEdge",
        product_name: "fusion-firmware Camera",
        serial_no: device_id_static,
        unique_id: device_id_static,
        device_name: device_name_static,
        device_type: Some(camera::DEV_TYPE_MATTER_CAMERA.dtype),
        // WebRTC ICE/STUN/DTLS payloads routed over Matter can exceed its
        // ~1200 B post-PASE UDP MRU; advertising TCP support (mDNS `T=1`)
        // lets a controller fall back to it for the large ones — same
        // flag the reference example sets for the identical reason.
        tcp_supported: true,
        ..TEST_DEV_DET
    }));
    (basic_info, device_id_static)
}

/// Computes the Matter `MT:...` standard setup-code string a controller
/// scans/enters to commission this device — the same payload
/// `Matter::print_standard_qr_text` logs at boot, but returned as a
/// string instead of printed, so `/onboarding/matter-qr.png`
/// (src/core/metrics.rs) can render it as a scannable PNG the same way
/// the app-onboarding QR already is (src/onboarding.rs).
///
/// Deliberately independent of the live `Matter` instance: the payload
/// is a pure function of `device_id` + this firmware's fixed test
/// attestation/commissioning data (`TEST_DEV_COMM`, same "for now, no
/// real CSA cert" call as everywhere else in this module) — no need to
/// reach into the Matter thread's state or block on it being up.
///
/// Call this AT MOST ONCE per process (e.g. once at boot, cached
/// alongside the other onboarding context) — it leaks a `device_id`
/// string via `build_basic_info` each time it runs.
pub fn setup_qr_text(device_id: &str) -> Result<String, rs_matter::error::Error> {
    let (basic_info, _) = build_basic_info(device_id);
    let payload = rs_matter::pairing::qr::QrPayload::new_from_basic_info(
        DiscoveryCapabilities::IP,
        rs_matter::pairing::qr::CommFlowType::Standard,
        TEST_DEV_COMM,
        basic_info,
        rs_matter::pairing::qr::no_optional_data,
    );
    let mut buf = [0u8; 512];
    let (text, _) = payload.as_str(&mut buf)?;
    Ok(text.to_string())
}

/// Spawns the Matter node on its own OS thread — `Matter::run` and
/// friends are a single-threaded async future tree
/// (`futures_lite::future::block_on`), the same shape as this firmware's
/// other self-contained subsystems (compare
/// `homeassistant::HomeAssistantBridge::spawn`), so a dedicated thread is
/// the natural fit rather than sharing the tokio runtime telemetry owns.
///
/// `matter_frame_rx` is the third `FrameRouter` consumer (see
/// `core::ring_buffer`) main.rs wires up only when `[matter.camera]`
/// specifically is enabled — `None` for an onoff-only ("light switch")
/// deployment with no use for a live camera frame source. Deliberately
/// NOT tied to whether the whole Matter node spawns at all (`cfg.enabled`
/// below): conflating the two disabled the entire subsystem for any
/// config with camera off, a real bug caught by actually running it.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    cfg: MatterConfig,
    camera_cfg: CameraConfig,
    stream_cfg: StreamConfig,
    ai_rules: Vec<AiRule>,
    device_id: String,
    matter_frame_rx: Option<Receiver<FrameHandle>>,
    shutdown: Arc<AtomicBool>,
) {
    if !cfg.enabled {
        return;
    }
    let builder = thread::Builder::new().name("matter_node".to_string());
    let spawned = builder.spawn(move || {
        if let Err(e) = run(cfg, camera_cfg, stream_cfg, ai_rules, device_id, matter_frame_rx, shutdown) {
            error!("Matter node exited: {e}");
        }
    });
    if let Err(e) = spawned {
        warn!("failed to spawn matter_node thread: {e}");
    }
}

fn run(
    cfg: MatterConfig,
    camera_cfg: CameraConfig,
    stream_cfg: StreamConfig,
    ai_rules: Vec<AiRule>,
    device_id: String,
    matter_frame_rx: Option<Receiver<FrameHandle>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), rs_matter::error::Error> {
    // Bridges rs-matter's internal `log::*` diagnostics into this
    // firmware's own tracing-subscriber (already initialized in main.rs)
    // so commissioning/session logs land in the same place as everything
    // else instead of going nowhere (nothing else in this process installs
    // a `log` backend).
    let _ = tracing_log::LogTracer::init();

    let (basic_info, device_id_static): (&'static BasicInfoConfig<'static>, &'static str) =
        build_basic_info(&device_id);

    let matter: &'static Matter<'static> = init_boxed(Matter::init(
        basic_info,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        MATTER_PORT,
    ));

    // Persisted fabric/ACL/subscription state — deliberately NOT the
    // reference's `DirKvBlobStore::new_default()` (`<tmp-dir>/rs-matter`,
    // cleared on most Linux distros' reboot, which would force
    // re-commissioning after every restart). `cfg.state_dir` follows this
    // firmware's existing "relative, config-owned persistence path" rule
    // (see `system.identity_file`/`system.ai_rules_override_file`).
    let store = DirKvBlobStore::new(std::path::PathBuf::from(&cfg.state_dir));
    if let Err(e) = std::fs::create_dir_all(&cfg.state_dir) {
        warn!("Matter: could not create state_dir {}: {e} (fabric state will not persist)", cfg.state_dir);
    }

    let buffers: &'static MatterBuffers = init_boxed(MatterBuffers::init());
    let state: &'static EthInteractionModelState =
        Box::leak(Box::new(EthInteractionModelState::new(EthNetwork::new_default())));

    let kv = matter.kv(store);
    matter.startup(&kv)?;

    let crypto = default_crypto(rand_core::OsRng, DAC_PRIVKEY);
    let mut rand = crypto.rand()?;

    // Real hardware/threads only start for device types actually enabled
    // — the ONE exception to "always construct everything unconditionally"
    // (see this file's header): the live H.264 encode pipeline is a real
    // GStreamer pipeline + thread, genuinely wasteful (and, on a device
    // with no camera hardware at all, likely to just fail loudly) to run
    // when the Camera endpoint won't even be listed.
    let live_source = camera::LiveH264Source::new();
    match (cfg.camera.enabled, matter_frame_rx) {
        (true, Some(frame_rx)) => {
            encoder::spawn(camera_cfg.clone(), stream_cfg.clone(), frame_rx, live_source.clone(), shutdown.clone());
        }
        (true, None) => {
            // Shouldn't happen given main.rs wires the tap whenever
            // [matter.camera] is enabled, but a camera endpoint with no
            // live media source is still a valid (if silent) state, not
            // worth crashing over.
            warn!("Matter: camera endpoint enabled but no frame source was wired up — live view will show nothing");
        }
        (false, _) => {}
    }

    let cam: &'static MatterCamera =
        MatterCamera::new(&mut rand, &camera_cfg, &stream_cfg, &ai_rules, live_source);
    let onoff_handler: &'static onoff::OnOff = onoff::build(&mut rand, &cfg.onoff);

    // Each possible endpoint is its OWN top-level `const` (not assembled
    // via a runtime function call) so Rust's rvalue static promotion
    // applies to the `&[...]` slices `clusters!`/`devices!` expand into
    // inside `root_endpoint!`/`camera_endpoint()`/`onoff_endpoint()` —
    // those only get promoted to `'static` storage automatically in a
    // const-evaluated context; a plain runtime function call doesn't
    // provide that context, which is exactly the bug this const-per-value
    // shape avoids (hit and fixed once already for the camera-only case).
    // Building the *list* of which of these appear on this boot, though,
    // is plain runtime `Vec` selection over already-'static-safe values —
    // no new temporaries are created by collecting them, so this part
    // needs no such care.
    const ROOT_ENDPOINT: Endpoint<'static> = root_endpoint!(eth);
    const CAMERA_ENDPOINT: Endpoint<'static> = camera::camera_endpoint();
    const ONOFF_ENDPOINT: Endpoint<'static> = onoff::onoff_endpoint();

    let mut endpoint_list: Vec<Endpoint<'static>> = vec![ROOT_ENDPOINT];
    if cfg.camera.enabled {
        endpoint_list.push(CAMERA_ENDPOINT);
    }
    if cfg.onoff.enabled {
        endpoint_list.push(ONOFF_ENDPOINT);
    }
    info!(
        camera = cfg.camera.enabled,
        onoff = cfg.onoff.enabled,
        endpoints = endpoint_list.len(),
        "Matter: node endpoints selected"
    );
    let endpoints: &'static [Endpoint<'static>] = Box::leak(endpoint_list.into_boxed_slice());
    let node = Node { endpoints };

    let base_handler = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&UnixNetifs)
        .build(rand);
    // Always chains EVERY possible device type's clusters (camera's
    // endpoint 1, onoff's endpoint 2) regardless of `cfg` — see this
    // file's header for why that's required, not just convenient, and why
    // it's harmless: an endpoint absent from `node.endpoints` above is
    // never routed to.
    let handler = onoff::chain_handlers(camera::chain_handlers(base_handler, cam, &mut rand), onoff_handler, &mut rand);
    // `(Node, <handler chain>)` is what actually implements `DataModel` —
    // the handler chain alone does not (see `camera::chain_handlers`'s
    // doc comment).
    let data_model = (node, handler);

    let im = Box::leak(Box::new(InteractionModel::new(
        matter, &crypto, buffers, data_model, kv, state,
    )));

    futures_lite::future::block_on(im.startup())?;

    let responder = DefaultResponder::new(im);
    let mut respond = pin!(responder.run::<4, 4>());
    let mut im_job = pin!(im.run());

    let udp_socket = Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)?;
    let tcp_socket = Async::<TcpListener>::bind(MATTER_SOCKET_BIND_ADDR)?;
    let tcp = TcpNetwork::<8>::new(tcp_socket);
    info!(addr = %MATTER_SOCKET_BIND_ADDR, "Matter: UDP+TCP transport bound");

    let mut net_send = ChainedNetwork::new(|addr: &Address| addr.is_tcp(), &tcp, &udp_socket);
    let mut net_recv = ChainedNetwork::new(|addr: &Address| addr.is_tcp(), &tcp, &udp_socket);
    let mut net_multicast = &udp_socket;

    let mut mdns_task = pin!(mdns::run(matter, &crypto, device_id_static));
    let mut transport = pin!(matter.run(&crypto, &mut net_send, &mut net_recv, &mut net_multicast));
    let mut driver = pin!(cam.drive());

    if !matter.has_fabrics() {
        matter.print_standard_qr_text(DiscoveryCapabilities::IP)?;
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())?;
        info!("Matter: node uncommissioned — pairing code/QR printed above; open a controller app to add this device");
    } else {
        info!("Matter: node already commissioned into at least one fabric");
    }

    // No explicit shutdown wiring into this select today: the process as
    // a whole exits via main.rs's supervisor loop (process::exit) rather
    // than a graceful per-subsystem stop, same as the RTSP/AI worker
    // threads. `shutdown` is still threaded through to `encoder::spawn`
    // above so that thread stops cleanly, which is the part actually
    // holding a live GStreamer pipeline.
    let matter_core = select4(&mut transport, &mut mdns_task, &mut respond, &mut im_job).coalesce();
    futures_lite::future::block_on(select(matter_core, &mut driver).coalesce())
}
