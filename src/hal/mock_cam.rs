use crate::config::CameraConfig;
use crate::core::error::EdgeResult;
use crate::hal::{FrameHandle, VideoSource};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Development camera for non-Linux hosts. Honors the configured resolution,
/// frame rate and pixel format, and renders a moving vertical bar over a
/// horizontal gradient so encoder/RTSP output is visually verifiable (a
/// static gray frame would encode to almost nothing).
pub struct MockCamera {
    width: usize,
    height: usize,
    frame_interval: Duration,
    layout: PixelLayout,
    buffer: Vec<u8>,
    /// One precomputed gradient row, copied per row each frame so the pattern
    /// costs a memcpy instead of a per-pixel loop.
    base_row: Vec<u8>,
    frame_counter: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum PixelLayout {
    /// Packed 4:2:2 — what UVC webcams and the YUYV config default produce.
    Yuy2,
    /// Semi-planar 4:2:0 — what Rockchip/NXP ISPs typically output.
    Nv12,
}

impl MockCamera {
    pub fn new(cfg: &CameraConfig) -> Self {
        let width = cfg.width as usize;
        let height = cfg.height as usize;
        let fps = cfg.fps.max(1);

        let layout = match cfg.format.to_uppercase().as_str() {
            "NV12" => PixelLayout::Nv12,
            "YUYV" | "YUY2" => PixelLayout::Yuy2,
            other => {
                warn!(
                    "Mock camera: unsupported format '{}', falling back to YUY2",
                    other
                );
                PixelLayout::Yuy2
            }
        };

        let (buffer, base_row) = match layout {
            PixelLayout::Yuy2 => {
                // Rows of [Y, U, Y, V] with a luma gradient and neutral chroma.
                let mut row = vec![128u8; width * 2];
                for col in 0..width {
                    row[col * 2] = ((col * 255) / width.max(1)) as u8;
                }
                (vec![128u8; width * height * 2], row)
            }
            PixelLayout::Nv12 => {
                // Full-res Y plane + half-res interleaved UV plane (value 128
                // = neutral chroma). base_row is one Y row.
                let mut row = vec![0u8; width];
                for (col, y) in row.iter_mut().enumerate() {
                    *y = ((col * 255) / width.max(1)) as u8;
                }
                (vec![128u8; width * height * 3 / 2], row)
            }
        };

        Self {
            width,
            height,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            layout,
            buffer,
            base_row,
            frame_counter: 0,
        }
    }

    fn render(&mut self) {
        let bar_center = ((self.frame_counter as usize) * 8) % self.width;
        let bar_half_width = 12usize;
        let bar_range = bar_center.saturating_sub(bar_half_width)
            ..(bar_center + bar_half_width).min(self.width);

        match self.layout {
            PixelLayout::Yuy2 => {
                let stride = self.width * 2;
                for row in 0..self.height {
                    let dst = &mut self.buffer[row * stride..row * stride + stride];
                    dst.copy_from_slice(&self.base_row);
                    for col in bar_range.clone() {
                        dst[col * 2] = 235; // bright luma bar
                    }
                }
            }
            PixelLayout::Nv12 => {
                let y_plane = &mut self.buffer[..self.width * self.height];
                for row in 0..self.height {
                    let dst = &mut y_plane[row * self.width..(row + 1) * self.width];
                    dst.copy_from_slice(&self.base_row);
                    dst[bar_range.clone()].fill(235);
                }
            }
        }
    }
}

impl VideoSource for MockCamera {
    fn initialize(&mut self) -> EdgeResult<()> {
        info!(
            width = self.width,
            height = self.height,
            "Mock Camera Initialized."
        );
        Ok(())
    }

    fn start_stream(&mut self) -> EdgeResult<()> {
        info!("Starting Mock Camera Stream (config-throttled)...");
        Ok(())
    }

    fn dequeue_frame(&mut self) -> EdgeResult<FrameHandle> {
        // Sleep to simulate the configured hardware capture rate.
        thread::sleep(self.frame_interval);

        self.frame_counter += 1;
        self.render();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        Ok(FrameHandle {
            id: self.frame_counter,
            data_ptr: self.buffer.as_ptr(),
            size: self.buffer.len(),
            timestamp_ns: now,
        })
    }

    fn stop_stream(&mut self) -> EdgeResult<()> {
        info!("Stopping Mock Camera.");
        Ok(())
    }
}
