use crate::config::CameraConfig;
use crate::core::error::EdgeResult;
use tracing::info;

// Load real drivers ONLY on Linux
#[cfg(target_os = "linux")]
pub mod generic_v4l2;
#[cfg(target_os = "linux")]
pub mod gst_v4l2;
#[cfg(target_os = "linux")]
pub mod nxp_isp;

// The dummy driver: implicit on non-Linux dev hosts (macOS/Windows), and
// explicitly selectable on Linux too via `camera.type = "MOCK"` — the only
// way to run the real firmware in a container/CI environment with no real
// camera hardware present (see iiotedge-cluster-sim's docker-compose test
// cluster). Plain std/Rust, no platform APIs, so it compiles everywhere.
pub mod mock_cam;

/// Userspace frame pool slots, shared by every backend that owns a raw pixel
/// buffer per slot (gst_v4l2, generic_v4l2). A `FrameHandle` is a zero-copy
/// pointer into one of these slots — `Clone` copies the pointer, not the
/// pixels — so a slot must not be reused while any handle referencing it is
/// still queued or being processed. Validated at config load
/// (`system.queue_capacity` must leave real margin under this) rather than
/// discovered as silently torn frame data in the field.
pub const FRAME_POOL_SIZE: usize = 8;

#[derive(Debug, Clone)]
pub struct FrameHandle {
    pub id: u64,
    pub data_ptr: *const u8,
    pub size: usize,
    // Read once encoder PTS stamping / event correlation land (Phases 3 & 7).
    #[allow(dead_code)]
    pub timestamp_ns: u64,
}

unsafe impl Send for FrameHandle {}
unsafe impl Sync for FrameHandle {}

pub trait VideoSource: Send + Sync {
    fn initialize(&mut self) -> EdgeResult<()>;
    fn start_stream(&mut self) -> EdgeResult<()>;
    fn dequeue_frame(&mut self) -> EdgeResult<FrameHandle>;
    fn stop_stream(&mut self) -> EdgeResult<()>;
}

type CameraCtor = fn(&CameraConfig) -> Box<dyn VideoSource>;

/// Camera backend registry: maps `[camera].type` strings to constructors.
/// Adding a new backend means implementing `VideoSource` and calling
/// `register_source()` once (see `register_builtins()` below) — nothing
/// about `create_camera()`'s own dispatch logic ever changes.
fn registry() -> &'static parking_lot::Mutex<std::collections::HashMap<&'static str, CameraCtor>> {
    static REGISTRY: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<&'static str, CameraCtor>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Register a camera backend under `type_name` (matched case-sensitively
/// against `[camera].type` in config). Re-registering the same `type_name`
/// silently replaces the previous entry — harmless, since `create_camera()`
/// calls `register_builtins()` fresh on every boot rather than requiring a
/// separate "register at startup" step callers have to remember.
pub(crate) fn register_source(type_name: &'static str, ctor: CameraCtor) {
    registry().lock().insert(type_name, ctor);
}

#[cfg(target_os = "linux")]
fn register_builtins() {
    register_source("NXP_ISP", |cfg| Box::new(nxp_isp::NxpIspCamera::new(cfg)));
    // Raw single-planar V4L2 (mmap ioctls) — UVC webcams and other classic
    // capture nodes. Lowest overhead path.
    register_source("SYSTEM_GENERIC", |cfg| {
        Box::new(generic_v4l2::GenericV4l2Camera::new(cfg))
    });
    // GStreamer v4l2src capture — handles multi-planar ISP nodes (Rockchip
    // rkisp1 on the Radxa Zero 3E) and compressed formats the raw backend
    // cannot. Recommended default on Rockchip; ROCKCHIP_RKISP is an alias.
    register_source("GST_V4L2", |cfg| {
        Box::new(gst_v4l2::GstV4l2Camera::new(cfg))
    });
    register_source("ROCKCHIP_RKISP", |cfg| {
        Box::new(gst_v4l2::GstV4l2Camera::new(cfg))
    });
    register_source("MOCK", |cfg| Box::new(mock_cam::MockCamera::new(cfg)));
}

#[cfg(not(target_os = "linux"))]
fn register_builtins() {
    register_source("MOCK", |cfg| Box::new(mock_cam::MockCamera::new(cfg)));
}

/// The Factory: Decides which driver to load at runtime based on config and
/// OS. Receives the full camera config so backends can negotiate resolution,
/// format and frame rate instead of taking whatever the driver last used.
pub fn create_camera(cfg: &CameraConfig) -> Box<dyn VideoSource> {
    info!("Requested camera driver: {}", cfg.r#type);
    register_builtins();

    #[cfg(not(target_os = "linux"))]
    {
        // Every non-Linux dev host gets the mock source regardless of the
        // configured type — none of the real Linux-only backends are even
        // compiled here, so there is nothing else `create_camera` could
        // return. A real deployment (always Linux) never hits this branch.
        if cfg.r#type != "MOCK" {
            info!(
                "Non-Linux OS detected (macOS/Windows). Injecting Mock Camera for local development."
            );
        }
        return registry().lock()["MOCK"](cfg);
    }

    #[cfg(target_os = "linux")]
    {
        let ctor = registry().lock().get(cfg.r#type.as_str()).copied();
        match ctor {
            // Explicit opt-in only — MOCK is never auto-selected on Linux
            // the way it is on dev hosts, so a real deployment can never
            // silently end up mock-only from a typo'd `type`.
            Some(ctor) => {
                if cfg.r#type == "MOCK" {
                    info!("camera.type=MOCK: using the synthetic test-pattern source, no hardware involved");
                }
                ctor(cfg)
            }
            None => panic!(
                "CRITICAL: Unsupported Linux camera type '{}' in config!",
                cfg.r#type
            ),
        }
    }
}

/// Log every /dev/video* node with its driver and capability flags. Called
/// from capture-backend failure paths so a misconfigured `camera.device_node`
/// can be corrected straight from the boot log.
#[cfg(target_os = "linux")]
pub(crate) fn scan_video_nodes() {
    info!("Scanning /dev/video* nodes to help pick camera.device_node:");
    for index in 0..=31 {
        let path = format!("/dev/video{index}");
        if !std::path::Path::new(&path).exists() {
            continue;
        }
        match v4l::Device::with_path(&path).and_then(|dev| dev.query_caps()) {
            Ok(caps) => info!(
                "  {path}: driver={} card={} capabilities={:?}",
                caps.driver, caps.card, caps.capabilities
            ),
            Err(e) => info!("  {path}: unreadable ({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(camera_type: &str) -> CameraConfig {
        CameraConfig {
            r#type: camera_type.to_string(),
            device_node: String::new(),
            width: 640,
            height: 480,
            fps: 15,
            format: "NV12".to_string(),
            auto_exposure: true,
            exposure_time_us: 0,
            gain: 0,
            source_params: String::new(),
        }
    }

    #[test]
    fn registry_resolves_mock_on_every_platform() {
        register_builtins();
        // MOCK is registered unconditionally (Linux and non-Linux builds
        // both include it) — this is the one backend every platform must
        // be able to construct.
        assert!(registry().lock().contains_key("MOCK"));
        let _camera = create_camera(&cfg("MOCK"));
    }

    #[test]
    fn register_source_overwrites_rather_than_duplicating() {
        fn ctor_a(cfg: &CameraConfig) -> Box<dyn VideoSource> {
            Box::new(mock_cam::MockCamera::new(cfg))
        }
        register_source("TEST_BACKEND", ctor_a);
        register_source("TEST_BACKEND", ctor_a);
        // Single lock acquisition: `assert_eq!(registry().lock().len(), ...)`
        // would deadlock here — its match-scrutinee tuple extends both
        // `.lock()` temporaries' lifetimes across the whole macro, so the
        // second call blocks forever on the (non-reentrant) guard the first
        // call is still holding.
        let guard = registry().lock();
        assert_eq!(guard.len(), guard.keys().len());
    }
}
