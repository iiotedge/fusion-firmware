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

/// The Factory: Decides which driver to load at runtime based on the OS.
/// Receives the full camera config so backends can negotiate resolution,
/// format and frame rate instead of taking whatever the driver last used.
pub fn create_camera(cfg: &CameraConfig) -> Box<dyn VideoSource> {
    info!("Requested camera driver: {}", cfg.r#type);

    #[cfg(target_os = "linux")]
    {
        match cfg.r#type.as_str() {
            "NXP_ISP" => Box::new(nxp_isp::NxpIspCamera::new(cfg)),
            // Raw single-planar V4L2 (mmap ioctls) — UVC webcams and other
            // classic capture nodes. Lowest overhead path.
            "SYSTEM_GENERIC" => Box::new(generic_v4l2::GenericV4l2Camera::new(cfg)),
            // GStreamer v4l2src capture — handles multi-planar ISP nodes
            // (Rockchip rkisp1 on the Radxa Zero 3E) and compressed formats
            // that the raw backend cannot. Recommended default on Rockchip.
            "GST_V4L2" | "ROCKCHIP_RKISP" => Box::new(gst_v4l2::GstV4l2Camera::new(cfg)),
            // Explicit opt-in only — never auto-selected on Linux the way it
            // is on dev hosts, so a real deployment can never silently end
            // up mock-only from a typo'd `type`.
            "MOCK" => {
                info!("camera.type=MOCK: using the synthetic test-pattern source, no hardware involved");
                Box::new(mock_cam::MockCamera::new(cfg))
            }
            other => panic!(
                "CRITICAL: Unsupported Linux camera type '{}' in config!",
                other
            ),
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!(
            "Non-Linux OS detected (macOS/Windows). Injecting Mock Camera for local development."
        );
        Box::new(mock_cam::MockCamera::new(cfg))
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
