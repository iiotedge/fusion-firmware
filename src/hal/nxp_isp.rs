use crate::config::CameraConfig;
use crate::core::error::EdgeResult;
use crate::hal::generic_v4l2::GenericV4l2Camera;
use crate::hal::{FrameHandle, VideoSource};
use tracing::info;

/// NXP i.MX 8M Plus ISP camera.
///
/// The ISP (isp-imx / vvcam) exposes its processed output as a standard V4L2
/// capture node (typically /dev/video2), so capture itself rides the generic
/// V4L2 backend. ISP-specific work — loading calibration binaries, dewarp,
/// 3A tuning via the vvext controls — is the HAL v2 scope (TODO.md Phase 2).
pub struct NxpIspCamera {
    inner: GenericV4l2Camera,
}

impl NxpIspCamera {
    pub fn new(cfg: &CameraConfig) -> Self {
        Self {
            inner: GenericV4l2Camera::new(cfg),
        }
    }
}

impl VideoSource for NxpIspCamera {
    fn initialize(&mut self) -> EdgeResult<()> {
        info!("Initializing NXP i.MX 8M Plus ISP capture node");
        self.inner.initialize()
    }

    fn start_stream(&mut self) -> EdgeResult<()> {
        self.inner.start_stream()
    }

    fn dequeue_frame(&mut self) -> EdgeResult<FrameHandle> {
        self.inner.dequeue_frame()
    }

    fn stop_stream(&mut self) -> EdgeResult<()> {
        self.inner.stop_stream()
    }
}
