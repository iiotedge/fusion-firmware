use crate::config::CameraConfig;
use crate::core::error::{EdgeError, EdgeResult};
use crate::hal::{FrameHandle, VideoSource, FRAME_POOL_SIZE};

use v4l::buffer::Type;
use v4l::io::traits::{CaptureStream, Stream as IoStream};
use v4l::prelude::*;
use v4l::video::capture::Parameters;
use v4l::video::Capture;
use v4l::{Format, FourCC};

use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, instrument, warn};

/// Number of mmap buffers negotiated with the kernel driver.
const DRIVER_BUFFER_COUNT: u32 = 4;

/// Generic V4L2 capture backend (UVC webcams, ISP capture nodes, …).
///
/// Phase 0 correctness note: frames are copied out of the kernel mmap buffer
/// into a rotating userspace pool so the DMA slot can be re-queued to the
/// driver immediately. True zero-copy (DMABUF export to VPU/NPU) is the HAL v2
/// work tracked in TODO.md Phase 2.
pub struct GenericV4l2Camera {
    path: String,
    requested: Format,
    requested_fps: u32,
    device: Option<Device>,
    stream: Option<MmapStream<'static>>,
    frame_pool: Vec<Vec<u8>>,
    pool_cursor: usize,
    frame_counter: u64,
}

impl GenericV4l2Camera {
    pub fn new(cfg: &CameraConfig) -> Self {
        Self {
            path: cfg.device_node.clone(),
            requested: Format::new(cfg.width, cfg.height, config_fourcc(&cfg.format)),
            requested_fps: cfg.fps.max(1),
            device: None,
            stream: None,
            frame_pool: (0..FRAME_POOL_SIZE).map(|_| Vec::new()).collect(),
            pool_cursor: 0,
            frame_counter: 0,
        }
    }
}

fn format_summary(f: &Format) -> String {
    format!("{}x{}@{}", f.width, f.height, f.fourcc)
}

/// Map the config's format string ("NV12", "YUYV", "MJPG", …) to a V4L2
/// FourCC. Unknown strings pass their first four bytes through so exotic
/// sensor formats stay expressible from config.
fn config_fourcc(format: &str) -> FourCC {
    let up = format.to_uppercase();
    let bytes = match up.as_str() {
        "YUY2" => *b"YUYV",
        _ => {
            let mut buf = *b"    ";
            for (dst, src) in buf.iter_mut().zip(up.bytes()) {
                *dst = src;
            }
            buf
        }
    };
    FourCC::new(&bytes)
}

impl VideoSource for GenericV4l2Camera {
    #[instrument(skip(self))]
    fn initialize(&mut self) -> EdgeResult<()> {
        let dev = Device::with_path(&self.path)
            .map_err(|e| EdgeError::HardwareFault(format!("open {}: {}", self.path, e)))?;

        let caps = dev
            .query_caps()
            .map_err(|e| EdgeError::HardwareFault(format!("query caps: {}", e)))?;

        // Many /dev/video* nodes are not cameras (codec M2M nodes, metadata
        // nodes) and ISPs like Rockchip's rkisp1 are multi-planar, which this
        // raw backend does not speak. Diagnose before set_format so the log
        // says what the node actually is.
        use v4l::capability::Flags;
        if !caps.capabilities.contains(Flags::VIDEO_CAPTURE) {
            let hint = if caps.capabilities.contains(Flags::VIDEO_CAPTURE_MPLANE) {
                "multi-planar capture node (typical for Rockchip rkisp1) — set camera.type = \"GST_V4L2\""
            } else if caps
                .capabilities
                .intersects(Flags::VIDEO_M2M | Flags::VIDEO_M2M_MPLANE)
            {
                "memory-to-memory codec node (decoder/encoder), not a camera — point device_node at a capture node"
            } else {
                "node has no single-planar video capture capability"
            };
            crate::hal::scan_video_nodes();
            return Err(EdgeError::HardwareFault(format!(
                "{} (driver={}, card={}): {}",
                self.path, caps.driver, caps.card, hint
            )));
        }

        // Negotiate the configured format; the driver answers with the
        // closest thing it can actually do.
        let actual = dev.set_format(&self.requested).map_err(|e| {
            match dev.enum_formats() {
                Ok(formats) if !formats.is_empty() => {
                    let supported: Vec<String> =
                        formats.iter().map(|f| f.fourcc.to_string()).collect();
                    warn!(
                        "{} supports pixel formats: {}",
                        self.path,
                        supported.join(", ")
                    );
                }
                _ => {}
            }
            crate::hal::scan_video_nodes();
            EdgeError::HardwareFault(format!("set format on {}: {}", self.path, e))
        })?;
        if actual.width != self.requested.width
            || actual.height != self.requested.height
            || actual.fourcc != self.requested.fourcc
        {
            warn!(
                requested = %format_summary(&self.requested),
                actual = %format_summary(&actual),
                "Driver adjusted the requested capture format"
            );
        }

        if let Err(e) = dev.set_params(&Parameters::with_fps(self.requested_fps)) {
            warn!("Driver rejected fps={} : {}", self.requested_fps, e);
        }

        info!(
            driver = %caps.driver,
            card = %caps.card,
            format = %format_summary(&actual),
            "Generic V4L2 camera initialized"
        );

        self.device = Some(dev);
        Ok(())
    }

    fn start_stream(&mut self) -> EdgeResult<()> {
        let dev = self
            .device
            .as_ref()
            .ok_or_else(|| EdgeError::HardwareFault("start_stream before initialize".into()))?;

        // Pre-size the pool slots to the driver's image size so steady-state
        // capture never reallocates (a realloc would move data out from under
        // an in-flight FrameHandle).
        let image_size = dev.format().map(|f| f.size as usize).unwrap_or(0);
        for slot in &mut self.frame_pool {
            slot.reserve(image_size);
        }

        // Buffer queueing and VIDIOC_STREAMON happen lazily on the first
        // CaptureStream::next() call — the crate's canonical init path.
        let stream = MmapStream::with_buffers(dev, Type::VideoCapture, DRIVER_BUFFER_COUNT)
            .map_err(|e| EdgeError::HardwareFault(format!("allocate mmap buffers: {}", e)))?;
        self.stream = Some(stream);

        info!(buffers = DRIVER_BUFFER_COUNT, "V4L2 capture stream ready");
        Ok(())
    }

    fn dequeue_frame(&mut self) -> EdgeResult<FrameHandle> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| EdgeError::HardwareFault("dequeue before start_stream".into()))?;

        let (buf, meta) = stream
            .next()
            .map_err(|e| EdgeError::HardwareFault(format!("VIDIOC_DQBUF: {}", e)))?;

        // For raw formats bytesused == sizeimage; guard against drivers that
        // report 0 or garbage.
        let used = meta.bytesused as usize;
        let len = if used == 0 || used > buf.len() {
            buf.len()
        } else {
            used
        };

        // Driver timestamps are monotonic-clock based; fall back to wall clock
        // for drivers that don't fill them in.
        let mut timestamp_ns = meta.timestamp.sec.max(0) as u64 * 1_000_000_000
            + meta.timestamp.usec.max(0) as u64 * 1_000;
        if timestamp_ns == 0 {
            warn!("Driver returned zero timestamp; falling back to system clock");
            timestamp_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
        }

        let cursor = self.pool_cursor;
        self.pool_cursor = (self.pool_cursor + 1) % FRAME_POOL_SIZE;
        let slot = &mut self.frame_pool[cursor];
        slot.clear();
        slot.extend_from_slice(&buf[..len]);

        self.frame_counter += 1;
        Ok(FrameHandle {
            id: self.frame_counter,
            data_ptr: slot.as_ptr(),
            size: slot.len(),
            timestamp_ns,
        })
    }

    fn stop_stream(&mut self) -> EdgeResult<()> {
        if let Some(mut stream) = self.stream.take() {
            stream
                .stop()
                .map_err(|e| EdgeError::HardwareFault(format!("VIDIOC_STREAMOFF: {}", e)))?;
        }
        info!("V4L2 capture stream stopped");
        Ok(())
    }
}
