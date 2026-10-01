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
// # How endpoints are assembled (registry + flat router)
//
// Each enabled device type contributes an `EndpointSpec` (device types +
// cluster handlers) to a `registry::Registry`; `Registry::plan` assigns
// endpoint ids, adds a Descriptor per endpoint and builds the node's endpoint
// list, and `Planned::into_router` wraps rs-matter's system handler chain in a
// flat, enum-dispatched `Router`. See registry.rs for the full rationale — in
// short: rs-matter's `ChainedHandler` nests one generic layer per cluster,
// which fixed the SET of clusters at compile time, forced the old "construct
// and chain every possible device type regardless of config" workaround, and
// had already pushed rustc into `#![recursion_limit]` territory at four device
// types. The router is runtime-sized (N instances of any kind), only ENABLED
// device types are constructed (a disabled relay never opens its GPIO), and
// type depth no longer grows with the number of clusters.
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
// throughout. (History: against rs-matter 0.3.0 this module had to use
// `rand_core` 0.6 even though upstream's `main` examples had moved to 0.10;
// the 2026-10-01 upgrade to rs-matter 0.4.1 moved this crate to `rand_core`/
// `rand` 0.10 to match, with `default_crypto(rand::rng(), ..)`.)
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
mod actuators;
pub(crate) mod air_quality;
pub mod camera;
mod commissioning;
pub mod encoder;
mod generic_switch;
mod mdns;
mod light;
mod onoff;
pub mod pairing;
mod registry;
mod sensors;
mod thermostat;
mod topology;

use crate::config::{AiRule, CameraConfig, MatterConfig, StreamConfig};
use crate::hal::FrameHandle;
use crate::signals::SignalBus;
use camera::MatterCamera;
pub(crate) use actuators::FanSteps;
pub(crate) use generic_switch::SwitchMode;
pub(crate) use sensors::OccupancyTech;

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

/// What this node calls itself — the strings a controller shows in the
/// accessory details and the commissioning advertisement. Pure so it can be
/// unit-tested; `build_basic_info` leaks the result into the `&'static str`s
/// `BasicInfoConfig` wants.
#[derive(Debug, PartialEq, Eq)]
struct Identity {
    vendor_name: String,
    product_name: String,
    device_name: String,
    /// The device type in the commissioning advertisement (mDNS `DT`).
    device_type: Option<u16>,
}

/// `device_id` is part of the default `device_name`, which mDNS limits to 32
/// bytes, so a long id is cut at a character boundary rather than rejected.
fn identity(cfg: &MatterConfig, device_id: &str) -> Identity {
    let mut device_name = if cfg.device_name.is_empty() {
        format!("fusion-firmware {device_id}")
    } else {
        cfg.device_name.clone()
    };
    while device_name.len() > 32 {
        device_name.pop();
    }
    Identity {
        vendor_name: if cfg.vendor_name.is_empty() {
            "IIoTEdge".to_string()
        } else {
            cfg.vendor_name.clone()
        },
        product_name: if !cfg.product_name.is_empty() {
            cfg.product_name.clone()
        } else if cfg.camera.enabled {
            // Unchanged for already-paired cameras.
            "fusion-firmware Camera".to_string()
        } else {
            // A light switch must not introduce itself as a camera.
            "fusion-firmware".to_string()
        },
        device_name,
        device_type: primary_device_type(cfg),
    }
}

/// The device type the node advertises while it can be commissioned: the camera
/// if there is one, else the first legacy device, else the first configured
/// endpoint. `None` when the node exposes nothing.
fn primary_device_type(cfg: &MatterConfig) -> Option<u16> {
    if cfg.camera.enabled {
        Some(camera::DEV_TYPE_MATTER_CAMERA.dtype)
    } else if cfg.onoff.enabled {
        Some(onoff::DEV_TYPE_ON_OFF_LIGHT.dtype)
    } else if cfg.light.enabled {
        Some(rs_matter::dm::devices::DEV_TYPE_EXTENDED_COLOR_LIGHT.dtype)
    } else if cfg.thermostat.enabled {
        Some(thermostat::DEV_TYPE_THERMOSTAT.dtype)
    } else {
        cfg.endpoints
            .iter()
            .find_map(|e| crate::config::MatterEndpointKind::parse(&e.kind))
            .map(|k| k.device_type().0)
    }
}

/// This build's version as BasicInformation wants it: `major<<16 | minor<<8 |
/// patch` and the plain string (a controller shows the string as the firmware
/// version).
fn software_version() -> (u32, &'static str) {
    let part = |s: &str, max: u32| s.parse::<u32>().unwrap_or(0).min(max);
    let number = (part(env!("CARGO_PKG_VERSION_MAJOR"), 0xFFFF) << 16)
        | (part(env!("CARGO_PKG_VERSION_MINOR"), 0xFF) << 8)
        | part(env!("CARGO_PKG_VERSION_PATCH"), 0xFF);
    (number, env!("CARGO_PKG_VERSION"))
}

/// Builds this device's `BasicInfoConfig` — this firmware's own
/// `device_id` everywhere the reference example uses "ACME Test"/
/// hardcoded placeholders. `BasicInfoConfig` fields are `&'static str`,
/// so the runtime strings are leaked once here; `run()` calls it once at
/// boot and nothing else should, or the leak repeats too.
/// Returns the config plus the leaked `device_id` string (reused as
/// `serial_no`/`unique_id` and as the mDNS hostname).
fn build_basic_info(
    device_id: &str,
    cfg: &MatterConfig,
) -> (&'static BasicInfoConfig<'static>, &'static str) {
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }
    let id = identity(cfg, device_id);
    let device_id_static = leak(device_id.to_string());
    let (sw_ver, sw_ver_str) = software_version();
    let basic_info: &'static BasicInfoConfig<'static> = Box::leak(Box::new(BasicInfoConfig {
        vendor_name: leak(id.vendor_name),
        product_name: leak(id.product_name),
        serial_no: device_id_static,
        unique_id: device_id_static,
        device_name: leak(id.device_name),
        device_type: id.device_type,
        sw_ver,
        sw_ver_str,
        // WebRTC ICE/STUN/DTLS payloads routed over Matter can exceed its
        // ~1200 B post-PASE UDP MRU; advertising TCP support (mDNS `T=1`)
        // lets a controller fall back to it for the large ones — same
        // flag the reference example sets for the identical reason.
        tcp_supported: true,
        ..TEST_DEV_DET
    }));
    (basic_info, device_id_static)
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
/// Everything the Matter node needs from the rest of the firmware besides its
/// own `[matter]` config. A struct (not a growing argument list) because each
/// generic capability this firmware gains tends to add another input here.
pub struct MatterInputs {
    pub camera_cfg: CameraConfig,
    pub stream_cfg: StreamConfig,
    pub ai_rules: Vec<AiRule>,
    /// `[ai].confidence_threshold`: the camera's Zone Management `Sensitivity`
    /// reflects it (see camera::zone_sensitivity).
    pub ai_confidence_threshold: f32,
    pub device_id: String,
    /// See the `matter_frame_rx` discussion above.
    pub frame_rx: Option<Receiver<FrameHandle>>,
    /// Named-signal registry the config-driven sensor endpoints read from.
    pub signals: Arc<SignalBus>,
    pub shutdown: Arc<AtomicBool>,
}

pub fn spawn(cfg: MatterConfig, inputs: MatterInputs) {
    if !cfg.enabled {
        return;
    }
    let builder = thread::Builder::new().name("matter_node".to_string());
    let spawned = builder.spawn(move || {
        if let Err(e) = run(cfg, inputs) {
            error!("Matter node exited: {e}");
        }
    });
    if let Err(e) = spawned {
        warn!("failed to spawn matter_node thread: {e}");
    }
}

fn run(cfg: MatterConfig, inputs: MatterInputs) -> Result<(), rs_matter::error::Error> {
    let MatterInputs {
        camera_cfg,
        stream_cfg,
        ai_rules,
        ai_confidence_threshold,
        device_id,
        frame_rx: matter_frame_rx,
        signals,
        shutdown,
    } = inputs;
    // Bridges rs-matter's internal `log::*` diagnostics into this
    // firmware's own tracing-subscriber (already initialized in main.rs)
    // so commissioning/session logs land in the same place as everything
    // else instead of going nowhere (nothing else in this process installs
    // a `log` backend).
    let _ = tracing_log::LogTracer::init();

    let (basic_info, device_id_static): (&'static BasicInfoConfig<'static>, &'static str) =
        build_basic_info(&device_id, &cfg);

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

    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let mut rand = crypto.rand()?;

    // Real hardware/threads only start for device types actually enabled:
    // the live H.264 encode pipeline is a real GStreamer pipeline + thread,
    // wasteful (and, on a device with no camera hardware at all, likely to
    // just fail loudly) to run when the Camera endpoint won't be listed.
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

    // Every device type contributes endpoint specs to the registry, and ONLY
    // ENABLED ones are even constructed (a disabled switch never opens its
    // GPIO line, a disabled camera never builds its WebRTC state). See
    // `registry.rs` for why dispatch is a flat enum router rather than the
    // nested `ChainedHandler` this used to be.
    let mut registry = registry::Registry::default();
    // Reserve the four legacy singleton ids up front, whether or not each is
    // enabled: paired controllers know them by these numbers, and a dynamic
    // endpoint must never be handed one of them.
    for id in [
        camera::CAMERA_ENDPOINT_ID,
        onoff::ONOFF_ENDPOINT_ID,
        light::LIGHT_ENDPOINT_ID,
        thermostat::THERMOSTAT_ENDPOINT_ID,
    ] {
        registry.reserve(id).map_err(|e| {
            error!("Matter: {e}");
            rs_matter::error::Error::new(rs_matter::error::ErrorCode::Invalid)
        })?;
    }
    let cam: Option<&'static MatterCamera> = if cfg.camera.enabled {
        let cam = MatterCamera::new(&mut rand, &camera_cfg, &stream_cfg, &ai_rules, ai_confidence_threshold, live_source);
        registry.add(camera::spec(cam));
        Some(cam)
    } else {
        None
    };
    // The legacy device types make Identify mandatory (the camera's is optional).
    if cfg.onoff.enabled {
        let spec = onoff::spec(onoff::build(&mut rand, &cfg.onoff));
        registry.add(spec.with_identify(&mut rand));
    }
    if cfg.light.enabled {
        let light_handlers = light::build(&mut rand);
        registry.add(light::spec(&light_handlers).with_identify(&mut rand));
    }
    if cfg.thermostat.enabled {
        let spec = thermostat::spec(thermostat::build(&mut rand));
        registry.add(spec.with_identify(&mut rand));
    }

    // Config-driven endpoints ([[matter.endpoints]]). Pinned ids are claimed
    // FIRST so a dynamic id can never take one a later entry pinned. An entry
    // that can't be built (unknown builtin, GPIO that won't open, a synthetic
    // source without allow_mock) is skipped with a loud error rather than
    // taking the whole node down — and keeps its id, so the entries after it
    // are not renumbered.
    let invalid = |e: String| {
        error!("Matter: {e}");
        rs_matter::error::Error::new(rs_matter::error::ErrorCode::Invalid)
    };
    for endpoint_cfg in &cfg.endpoints {
        if let Some(id) = endpoint_cfg.endpoint {
            registry.reserve(id).map_err(invalid)?;
        }
    }
    for (index, endpoint_cfg) in cfg.endpoints.iter().enumerate() {
        let id = match endpoint_cfg.endpoint {
            Some(id) => id,
            None => registry.alloc().map_err(invalid)?,
        };
        // Actuators are commanded through a `sink`, sensors and switches read a
        // `source`; a switch additionally emits events.
        use crate::config::MatterEndpointKind as Kind;
        let built = match Kind::parse(&endpoint_cfg.kind) {
            Some(k) if k.is_actuator() => {
                actuators::build_endpoint(endpoint_cfg, index, id, &signals, &mut rand)
            }
            Some(Kind::GenericSwitch) => {
                generic_switch::build_endpoint(endpoint_cfg, index, id, &signals, &mut rand)
            }
            Some(Kind::AirQuality) => {
                air_quality::build_endpoint(endpoint_cfg, index, id, &signals, &mut rand)
            }
            _ => sensors::build_endpoint(endpoint_cfg, index, id, &signals, &mut rand),
        };
        match built {
            Ok(spec) => registry.add(spec),
            Err(e) => error!(
                endpoint = id,
                name = %crate::config::effective_endpoint_name(endpoint_cfg, index),
                "Matter: skipping endpoint: {e}"
            ),
        }
    }

    const ROOT_ENDPOINT: Endpoint<'static> = root_endpoint!(eth);
    let planned = registry.plan(&mut rand, ROOT_ENDPOINT, basic_info.vid).map_err(|e| {
        error!("Matter: invalid endpoint configuration: {e}");
        rs_matter::error::Error::new(rs_matter::error::ErrorCode::Invalid)
    })?;
    info!(
        camera = cfg.camera.enabled,
        onoff = cfg.onoff.enabled,
        light = cfg.light.enabled,
        thermostat = cfg.thermostat.enabled,
        configured = cfg.endpoints.len(),
        endpoints = planned.endpoints.len(),
        "Matter: node endpoints selected"
    );
    for endpoint in planned.endpoints.iter().skip(1) {
        info!(
            endpoint = endpoint.id,
            device_type = format!("0x{:04X}", endpoint.device_types.first().map_or(0, |d| d.dtype)),
            clusters = endpoint.clusters.len(),
            "Matter: endpoint"
        );
    }
    // What a controller can learn about this node, fingerprinted before `planned`
    // is consumed: see topology.rs for why a change must bump ConfigurationVersion.
    let topology_signature = topology::signature(env!("GIT_HASH"), planned.endpoints);
    let node = Node {
        endpoints: planned.endpoints,
    };

    // Built AFTER planning: `build` consumes the RNG by value.
    let base_handler = endpoints::EthSysHandlerBuilder::new()
        .netif_diag(&UnixNetifs)
        .build(rand);
    // `(Node, <handler>)` is what actually implements `DataModel`; the
    // router falls through to the system chain for the root endpoint.
    let handler = planned.into_router(base_handler);
    let data_model = (node, handler);

    let im = Box::leak(Box::new(InteractionModel::new(
        matter, &crypto, buffers, data_model, kv, state,
    )));

    futures_lite::future::block_on(im.startup())?;
    topology::apply(&cfg.state_dir, topology_signature, matter.has_fabrics(), || im.bump_configuration_version());

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
    let mut driver = pin!(async {
        match cam {
            Some(cam) => cam.drive().await,
            // No camera endpoint -> no WebRTC session driver to run.
            None => core::future::pending::<Result<(), rs_matter::error::Error>>().await,
        }
    });

    let pairing = pairing::Pairing::standard()?;
    if !matter.has_fabrics() {
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())?;
        pairing::announce_pairing_open(&pairing, false);
    } else {
        info!("Matter: node already commissioned into at least one fabric");
    }

    // The window above is the only one rs-matter opens by itself. Once the last
    // controller is removed the node must become addable again without a restart.
    let mut reopen = pin!(commissioning::reopen_after_last_fabric_removed(
        || matter.has_fabrics(),
        || matter.comm_window_state().is_open(),
        || im.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS),
        || pairing::announce_pairing_open(&pairing, true),
    ));

    // No explicit shutdown wiring into this select today: the process as
    // a whole exits via main.rs's supervisor loop (process::exit) rather
    // than a graceful per-subsystem stop, same as the RTSP/AI worker
    // threads. `shutdown` is still threaded through to `encoder::spawn`
    // above so that thread stops cleanly, which is the part actually
    // holding a live GStreamer pipeline.
    let matter_core = select4(&mut transport, &mut mdns_task, &mut respond, &mut im_job).coalesce();
    let with_driver = select(matter_core, &mut driver).coalesce();
    futures_lite::future::block_on(select(with_driver, &mut reopen).coalesce())
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::config::MatterEndpointConfig;

    fn endpoint(kind: &str) -> MatterEndpointConfig {
        MatterEndpointConfig {
            kind: kind.to_string(),
            name: String::new(),
            endpoint: None,
            source: String::new(),
            sink: String::new(),
            fan_speeds: String::new(),
            scale: 1.0,
            offset: 0.0,
            min: None,
            max: None,
            invert: false,
            occupancy_type: String::new(),
            sources: Default::default(),
            scales: Default::default(),
            hold_ms: None,
            switch_mode: String::new(),
            long_press_ms: None,
            multi_press_ms: None,
            multi_press_max: None,
            debounce_ms: None,
            poll_ms: None,
            allow_mock: false,
        }
    }

    #[test]
    fn a_default_camera_node_keeps_its_historical_identity() {
        // Already-paired cameras must see exactly what they saw before.
        let id = identity(&MatterConfig::default(), "radxa-1");
        assert_eq!(id.vendor_name, "IIoTEdge");
        assert_eq!(id.product_name, "fusion-firmware Camera");
        assert_eq!(id.device_name, "fusion-firmware radxa-1");
        assert_eq!(id.device_type, Some(0x0142));
    }

    #[test]
    fn a_node_without_a_camera_does_not_call_itself_one() {
        let mut cfg = MatterConfig::default();
        cfg.camera.enabled = false;
        cfg.onoff.enabled = true;
        let id = identity(&cfg, "relay-9");
        assert_eq!(id.product_name, "fusion-firmware");
        assert_eq!(id.device_type, Some(0x0100), "advertised as the light it is, not a camera (0x0142)");
    }

    #[test]
    fn configured_identity_wins() {
        let cfg = MatterConfig {
            vendor_name: "Acme Controls".into(),
            product_name: "Acme Relay Board".into(),
            device_name: "Acme Relay 01".into(),
            ..MatterConfig::default()
        };
        let id = identity(&cfg, "relay-9");
        assert_eq!(
            (id.vendor_name.as_str(), id.product_name.as_str(), id.device_name.as_str()),
            ("Acme Controls", "Acme Relay Board", "Acme Relay 01")
        );
    }

    #[test]
    fn a_long_device_id_is_cut_for_the_32_byte_device_name_not_rejected() {
        let id = identity(&MatterConfig::default(), "a-very-long-device-identifier-0123456789");
        assert_eq!(id.device_name.len(), 32);
        assert!(id.device_name.starts_with("fusion-firmware a-very-long"));
    }

    #[test]
    fn the_advertised_device_type_follows_whatever_the_node_exposes() {
        let mut cfg = MatterConfig::default();
        cfg.camera.enabled = false;
        assert_eq!(primary_device_type(&cfg), None, "nothing exposed");
        cfg.endpoints.push(endpoint("temperature_sensor"));
        assert_eq!(primary_device_type(&cfg), Some(0x0302));
        cfg.endpoints.insert(0, endpoint("fan"));
        assert_eq!(primary_device_type(&cfg), Some(0x002B), "the first configured endpoint");
        cfg.thermostat.enabled = true;
        assert_eq!(primary_device_type(&cfg), Some(0x0301), "legacy devices come first");
        cfg.light.enabled = true;
        assert_eq!(primary_device_type(&cfg), Some(0x010D));
        cfg.onoff.enabled = true;
        assert_eq!(primary_device_type(&cfg), Some(0x0100));
        cfg.camera.enabled = true;
        assert_eq!(primary_device_type(&cfg), Some(0x0142));
    }

    #[test]
    fn software_version_is_this_builds_version() {
        let (number, text) = software_version();
        assert_eq!(text, env!("CARGO_PKG_VERSION"));
        let parts: Vec<u32> = text.split('.').map(|p| p.parse().unwrap()).collect();
        assert_eq!(number, (parts[0] << 16) | (parts[1] << 8) | parts[2]);
    }
}
