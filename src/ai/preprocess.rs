// src/ai/preprocess.rs
//
// Frame → tensor: samples the camera's native pixel format (NV12 / YUY2)
// directly into a letterboxed NCHW f32 RGB tensor in a single pass over the
// model-input pixels — no intermediate full-frame RGB conversion. ROI-aware:
// when [ai].roi is set, only that window is fed to the model and detections
// are mapped back to full-frame coordinates.
use crate::ai::runtime::InputTensor;
use crate::config::{AiConfig, CameraConfig};
use crate::core::error::{EdgeError, EdgeResult};

/// YOLO-conventional letterbox padding gray (114/255).
const PAD: f32 = 114.0 / 255.0;

#[derive(Clone, Copy)]
enum PixelLayout {
    Nv12,
    Yuy2,
}

/// Geometry of one preprocessed frame: everything a parser needs to project
/// model-space boxes back onto the original frame.
#[derive(Debug, Clone, Copy)]
pub struct Letterbox {
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    pub roi_x: u32,
    pub roi_y: u32,
    pub frame_w: u32,
    pub frame_h: u32,
}

impl Letterbox {
    /// Map a model-input-space box (center + size) to frame pixels, clamped
    /// to the frame bounds.
    pub fn frame_box(&self, cx: f32, cy: f32, w: f32, h: f32) -> (u32, u32, u32, u32) {
        let x0 = ((cx - w / 2.0 - self.pad_x) / self.scale) + self.roi_x as f32;
        let y0 = ((cy - h / 2.0 - self.pad_y) / self.scale) + self.roi_y as f32;
        let bw = w / self.scale;
        let bh = h / self.scale;

        let x0 = x0.clamp(0.0, self.frame_w.saturating_sub(1) as f32);
        let y0 = y0.clamp(0.0, self.frame_h.saturating_sub(1) as f32);
        let bw = bw.clamp(0.0, self.frame_w as f32 - x0);
        let bh = bh.clamp(0.0, self.frame_h as f32 - y0);
        (x0 as u32, y0 as u32, bw as u32, bh as u32)
    }
}

pub struct Preprocessor {
    layout: PixelLayout,
    frame_w: usize,
    frame_h: usize,
    roi: (u32, u32, u32, u32),
    input_w: usize,
    input_h: usize,
    expected_frame_bytes: usize,
}

impl Preprocessor {
    pub fn new(camera: &CameraConfig, ai: &AiConfig) -> EdgeResult<Self> {
        let layout = match camera.format.to_uppercase().as_str() {
            "NV12" => PixelLayout::Nv12,
            "YUYV" | "YUY2" => PixelLayout::Yuy2,
            other => {
                return Err(EdgeError::AiPanic(format!(
                    "AI preprocessing supports NV12/YUYV frames; camera.format is '{other}'. \
                     Use a raw format or extend ai::preprocess."
                )))
            }
        };

        let (fw, fh) = (camera.width, camera.height);
        let roi = match ai.roi.as_slice() {
            [] => (0, 0, fw, fh),
            [x, y, w, h] => {
                if x + w > fw || y + h > fh || *w == 0 || *h == 0 {
                    return Err(EdgeError::AiPanic(format!(
                        "ai.roi [{x},{y},{w},{h}] exceeds the {fw}x{fh} frame"
                    )));
                }
                (*x, *y, *w, *h)
            }
            other => {
                return Err(EdgeError::AiPanic(format!(
                    "ai.roi must be [] or [x, y, w, h] (got {} values)",
                    other.len()
                )))
            }
        };

        let expected_frame_bytes = match layout {
            PixelLayout::Nv12 => fw as usize * fh as usize * 3 / 2,
            PixelLayout::Yuy2 => fw as usize * fh as usize * 2,
        };

        Ok(Self {
            layout,
            frame_w: fw as usize,
            frame_h: fh as usize,
            roi,
            input_w: ai.input_width.max(32) as usize,
            input_h: ai.input_height.max(32) as usize,
            expected_frame_bytes,
        })
    }

    /// Sample one frame into a letterboxed NCHW RGB tensor.
    pub fn run(&self, frame: &[u8]) -> EdgeResult<(InputTensor, Letterbox)> {
        if frame.len() < self.expected_frame_bytes {
            return Err(EdgeError::AiPanic(format!(
                "frame is {} bytes, expected at least {} for {}x{}",
                frame.len(),
                self.expected_frame_bytes,
                self.frame_w,
                self.frame_h
            )));
        }

        let (roi_x, roi_y, roi_w, roi_h) = self.roi;
        let scale = (self.input_w as f32 / roi_w as f32).min(self.input_h as f32 / roi_h as f32);
        let scaled_w = roi_w as f32 * scale;
        let scaled_h = roi_h as f32 * scale;
        let pad_x = (self.input_w as f32 - scaled_w) / 2.0;
        let pad_y = (self.input_h as f32 - scaled_h) / 2.0;

        let plane = self.input_w * self.input_h;
        let mut data = vec![PAD; 3 * plane];

        for oy in 0..self.input_h {
            let sy_f = (oy as f32 - pad_y) / scale;
            if sy_f < 0.0 || sy_f >= roi_h as f32 {
                continue; // letterbox padding row
            }
            let sy = roi_y as usize + sy_f as usize;
            for ox in 0..self.input_w {
                let sx_f = (ox as f32 - pad_x) / scale;
                if sx_f < 0.0 || sx_f >= roi_w as f32 {
                    continue; // letterbox padding column
                }
                let sx = roi_x as usize + sx_f as usize;

                let (y, u, v) = self.sample_yuv(frame, sx, sy);
                let (r, g, b) = yuv_to_rgb(y, u, v);
                let idx = oy * self.input_w + ox;
                data[idx] = r;
                data[plane + idx] = g;
                data[2 * plane + idx] = b;
            }
        }

        let tensor = InputTensor {
            data,
            shape: [1, 3, self.input_h as i64, self.input_w as i64],
        };
        let letterbox = Letterbox {
            scale,
            pad_x,
            pad_y,
            roi_x,
            roi_y,
            frame_w: self.frame_w as u32,
            frame_h: self.frame_h as u32,
        };
        Ok((tensor, letterbox))
    }

    #[inline]
    fn sample_yuv(&self, frame: &[u8], sx: usize, sy: usize) -> (u8, u8, u8) {
        match self.layout {
            PixelLayout::Nv12 => {
                let y = frame[sy * self.frame_w + sx];
                let uv_base =
                    self.frame_w * self.frame_h + (sy / 2) * self.frame_w + (sx & !1usize);
                (y, frame[uv_base], frame[uv_base + 1])
            }
            PixelLayout::Yuy2 => {
                // [Y0 U Y1 V] per 2-pixel group.
                let base = (sy * self.frame_w + sx) * 2;
                let group = base & !3usize;
                (frame[base], frame[group + 1], frame[group + 3])
            }
        }
    }
}

/// BT.601 limited-range YUV → normalized RGB.
#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (f32, f32, f32) {
    let y = f32::from(y) - 16.0;
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    let r = (1.164 * y + 1.596 * v).clamp(0.0, 255.0);
    let g = (1.164 * y - 0.392 * u - 0.813 * v).clamp(0.0, 255.0);
    let b = (1.164 * y + 2.017 * u).clamp(0.0, 255.0);
    (r / 255.0, g / 255.0, b / 255.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(w: u32, h: u32, format: &str) -> CameraConfig {
        CameraConfig {
            r#type: "MOCK".into(),
            device_node: String::new(),
            source_params: String::new(),
            width: w,
            height: h,
            fps: 30,
            format: format.into(),
            auto_exposure: false,
            exposure_time_us: 0,
            gain: 0,
        }
    }

    fn ai(input: u32, roi: Vec<u32>) -> AiConfig {
        AiConfig {
            enabled: true,
            model_path: String::new(),
            runtime: "onnx".into(),
            hardware_delegate: "CPU".into(),
            parser: "yolov8".into(),
            confidence_threshold: 0.5,
            nms_iou_threshold: 0.45,
            inference_fps_limit: 0,
            roi,
            input_width: input,
            input_height: input,
            labels: vec![],
            class_filter: vec![],
            intra_threads: 1,
            onnx_dylib_path: String::new(),
            test_hooks_enabled: false,
            rules: vec![],
        }
    }

    #[test]
    fn letterbox_roundtrip_maps_center_back_to_frame() {
        // 1920x1080 → 640x640: scale = 1/3, pad_y = (640-360)/2 = 140.
        let pre = Preprocessor::new(&camera(1920, 1080, "NV12"), &ai(640, vec![])).unwrap();
        let frame = vec![128u8; 1920 * 1080 * 3 / 2];
        let (_, lb) = pre.run(&frame).unwrap();
        assert!((lb.scale - 1.0 / 3.0).abs() < 1e-4);
        // A box centered mid-input maps to mid-frame.
        let (x, y, w, h) = lb.frame_box(320.0, 320.0, 96.0, 96.0);
        assert_eq!((x, y), (816, 396)); // 960-144, 540-144
        assert_eq!((w, h), (288, 288));
    }

    #[test]
    fn roi_offsets_are_applied() {
        let pre = Preprocessor::new(
            &camera(1920, 1080, "NV12"),
            &ai(640, vec![400, 200, 640, 640]),
        )
        .unwrap();
        let frame = vec![128u8; 1920 * 1080 * 3 / 2];
        let (_, lb) = pre.run(&frame).unwrap();
        assert_eq!(lb.scale, 1.0);
        let (x, y, _, _) = lb.frame_box(10.0, 10.0, 4.0, 4.0);
        assert_eq!((x, y), (408, 208));
    }

    #[test]
    fn rejects_bad_roi_and_compressed_formats() {
        assert!(
            Preprocessor::new(&camera(640, 480, "NV12"), &ai(640, vec![600, 0, 100, 100])).is_err()
        );
        assert!(Preprocessor::new(&camera(640, 480, "MJPG"), &ai(640, vec![])).is_err());
    }

    #[test]
    fn tensor_shape_and_padding_value() {
        let pre = Preprocessor::new(&camera(320, 180, "YUYV"), &ai(64, vec![])).unwrap();
        let frame = vec![235u8; 320 * 180 * 2];
        let (tensor, _) = pre.run(&frame).unwrap();
        assert_eq!(tensor.shape, [1, 3, 64, 64]);
        assert_eq!(tensor.data.len(), 3 * 64 * 64);
        // Top row is letterbox padding (aspect 16:9 into square).
        assert!((tensor.data[0] - PAD).abs() < 1e-6);
    }
}
