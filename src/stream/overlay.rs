// src/stream/overlay.rs
//
// Burned-in OSD overlays (F6). Two mechanisms, both fully config-driven:
//
//  * Text (timestamp / device id / custom line): GStreamer pango elements
//    (clockoverlay / textoverlay) inserted into the encode pipelines. Probed
//    at runtime — devices without the pango plugin log a hint and stream
//    without text rather than failing.
//  * AI detection boxes: drawn CPU-side into the raw frame *once*, before the
//    shared buffer fans out to RTSP and the NVR recorder, so live view and
//    recorded evidence are pixel-identical.
use crate::ai::engine::AiEvent;
use crate::config::{OverlayConfig, SystemConfig};
use crate::stream::widgets::{backdrop, draw_text};

use gstreamer as gst;
use parking_lot::Mutex;
use std::time::{Duration, Instant};
use tracing::warn;

/// Bright luma for box borders (BT.601 broadcast white).
const BOX_LUMA: u8 = 235;
const BOX_BORDER_PX: usize = 3;

/// Build the text-overlay launch fragment ("" when disabled or unavailable).
/// Overlay elements accept raw YUV directly, so this slots between
/// videoconvert and the encoder.
pub fn text_fragment(overlay: &OverlayConfig, system: &SystemConfig) -> String {
    if !overlay.enabled {
        return String::new();
    }

    let pango_available = gst::ElementFactory::find("textoverlay").is_some()
        && gst::ElementFactory::find("clockoverlay").is_some();
    if !pango_available {
        if overlay.show_timestamp || overlay.show_device_id || !overlay.custom_text.is_empty() {
            warn!(
                "Text overlays configured but GStreamer pango elements are missing \
                 (install gstreamer plugins-base pango support); continuing without text"
            );
        }
        return String::new();
    }

    let font = sanitize(&overlay.font);
    let mut fragments = Vec::new();

    if overlay.show_device_id {
        fragments.push(format!(
            "textoverlay text=\"{}\" halignment=left valignment=top font-desc=\"{font}\" shaded-background=true ! ",
            sanitize(&system.device_id)
        ));
    }
    if overlay.show_timestamp {
        fragments.push(format!(
            "clockoverlay time-format=\"{}\" halignment=right valignment=top font-desc=\"{font}\" shaded-background=true ! ",
            sanitize(&overlay.timestamp_format)
        ));
    }
    if !overlay.custom_text.trim().is_empty() {
        fragments.push(format!(
            "textoverlay text=\"{}\" halignment=left valignment=bottom font-desc=\"{font}\" shaded-background=true ! ",
            sanitize(overlay.custom_text.trim())
        ));
    }

    fragments.concat()
}

/// Launch-string sanitization: overlay text comes from config, but quotes or
/// exclamation marks would corrupt the pipeline description.
fn sanitize(text: &str) -> String {
    text.replace(['"', '!', '\\'], " ")
}

/// A box to burn into the frame, in full-frame pixel coordinates.
#[derive(Debug, Clone)]
pub struct DrawBox {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub label: String,
    pub confidence: f32,
}

/// Latest detections, shared between the AI thread (writer) and the media
/// thread (reader). Boxes expire after the configured TTL so a stalled AI
/// thread can't freeze stale boxes onto the evidence stream.
pub struct DetectionOverlay {
    ttl: Duration,
    state: Mutex<(Vec<DrawBox>, Instant)>,
}

impl DetectionOverlay {
    pub fn new(ttl_ms: u64) -> Self {
        Self {
            ttl: Duration::from_millis(ttl_ms.max(100)),
            state: Mutex::new((Vec::new(), Instant::now())),
        }
    }

    pub fn update(&self, events: &[AiEvent]) {
        let boxes = events
            .iter()
            .map(|e| DrawBox {
                x: e.x,
                y: e.y,
                w: e.w,
                h: e.h,
                label: e.label.clone(),
                confidence: e.confidence,
            })
            .collect();
        *self.state.lock() = (boxes, Instant::now());
    }

    /// Current boxes if still within TTL.
    pub fn active_boxes(&self) -> Vec<DrawBox> {
        let state = self.state.lock();
        if state.1.elapsed() > self.ttl {
            Vec::new()
        } else {
            state.0.clone()
        }
    }
}

/// Frame geometry needed to draw into raw YUV bytes.
#[derive(Clone, Copy)]
pub enum FrameLayout {
    Nv12 { width: usize, height: usize },
    Yuy2 { width: usize, height: usize },
}

impl FrameLayout {
    pub fn from_format(format: &str, width: u32, height: u32) -> Option<Self> {
        match format.to_uppercase().as_str() {
            "NV12" => Some(FrameLayout::Nv12 {
                width: width as usize,
                height: height as usize,
            }),
            "YUYV" | "YUY2" => Some(FrameLayout::Yuy2 {
                width: width as usize,
                height: height as usize,
            }),
            _ => None,
        }
    }

    fn dims(&self) -> (usize, usize) {
        match *self {
            FrameLayout::Nv12 { width, height } | FrameLayout::Yuy2 { width, height } => {
                (width, height)
            }
        }
    }

    #[inline]
    fn set_luma(&self, frame: &mut [u8], x: usize, y: usize) {
        self.set_luma_value(frame, x, y, BOX_LUMA);
    }

    /// Write one luma sample, bounds-checked against the frame dimensions
    /// (an out-of-range x/y must never index into the chroma plane).
    #[inline]
    pub(crate) fn set_luma_value(&self, frame: &mut [u8], x: usize, y: usize, value: u8) {
        let (w, h) = self.dims();
        if x >= w || y >= h {
            return;
        }
        let index = match *self {
            FrameLayout::Nv12 { width, .. } => y * width + x,
            FrameLayout::Yuy2 { width, .. } => (y * width + x) * 2,
        };
        if let Some(px) = frame.get_mut(index) {
            *px = value;
        }
    }
}

/// Burn box borders into the frame's luma plane. Chroma is untouched, so
/// borders render white-ish regardless of scene content — cheap and codec
/// friendly (only border pixels change between frames).
pub fn draw_boxes(frame: &mut [u8], layout: FrameLayout, boxes: &[DrawBox]) {
    let (fw, fh) = layout.dims();
    for b in boxes {
        let x0 = (b.x as usize).min(fw.saturating_sub(1));
        let y0 = (b.y as usize).min(fh.saturating_sub(1));
        let x1 = (b.x as usize + b.w as usize).min(fw);
        let y1 = (b.y as usize + b.h as usize).min(fh);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let border = BOX_BORDER_PX.min((y1 - y0).max(1)).min((x1 - x0).max(1));

        // Horizontal edges.
        for edge_row in 0..border {
            for x in x0..x1 {
                layout.set_luma(frame, x, y0 + edge_row);
                layout.set_luma(frame, x, y1 - 1 - edge_row);
            }
        }
        // Vertical edges.
        for y in y0..y1 {
            for edge_col in 0..border {
                layout.set_luma(frame, x0 + edge_col, y);
                layout.set_luma(frame, x1 - 1 - edge_col, y);
            }
        }

        // Label + confidence tag ("PERSON 72%"), dark backdrop for
        // readability against any scene content — same convention as the
        // machine-data widgets' header text (widgets.rs).
        const LABEL_SCALE: u32 = 2;
        const GLYPH_W: u32 = 6; // draw_text's per-character advance
        const GLYPH_H: u32 = 7; // font row count
        let text = format!("{} {:.0}%", b.label, b.confidence * 100.0);
        let text_w = text.chars().count() as u32 * GLYPH_W * LABEL_SCALE;
        let text_h = GLYPH_H * LABEL_SCALE + 2;
        // Sits just above the box; if that would run off the top of the
        // frame, drop inside the box top edge instead.
        let label_y = if y0 as u32 >= text_h + 2 {
            y0 as u32 - text_h - 2
        } else {
            y0 as u32 + border as u32 + 1
        };
        backdrop(frame, layout, x0 as u32, label_y, text_w, text_h);
        draw_text(
            frame,
            layout,
            x0 as u32 + 1,
            label_y + 1,
            &text,
            LABEL_SCALE,
        );
    }
}

/// Burn a thick warning band around the frame edge (tamper indication).
/// Luma-only like draw_boxes, so it reads as a bright frame on any scene.
pub fn draw_border(frame: &mut [u8], layout: FrameLayout, thickness: usize) {
    let (fw, fh) = layout.dims();
    let t = thickness.min(fw / 4).min(fh / 4).max(1);
    for y in 0..fh {
        let edge_row = y < t || y >= fh - t;
        if edge_row {
            for x in 0..fw {
                layout.set_luma(frame, x, y);
            }
        } else {
            for x in 0..t {
                layout.set_luma(frame, x, y);
                layout.set_luma(frame, fw - 1 - x, y);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_band_is_thick_and_bounded() {
        let layout = FrameLayout::Nv12 {
            width: 64,
            height: 32,
        };
        let mut frame = vec![0u8; 64 * 32 * 3 / 2];
        draw_border(&mut frame, layout, 4);
        assert_eq!(frame[0], BOX_LUMA); // top-left corner
        assert_eq!(frame[3 * 64 + 63], BOX_LUMA); // row 3 right edge
        assert_eq!(frame[16 * 64 + 32], 0); // center untouched
        assert!(frame[64 * 32..].iter().all(|&px| px == 0)); // chroma untouched
    }

    #[test]
    fn draws_border_inside_frame_only() {
        let layout = FrameLayout::Nv12 {
            width: 64,
            height: 32,
        };
        let mut frame = vec![0u8; 64 * 32 * 3 / 2];
        // Box partially out of bounds on the right/bottom.
        draw_boxes(
            &mut frame,
            layout,
            &[DrawBox {
                x: 50,
                y: 20,
                w: 100,
                h: 100,
                label: "person".into(),
                confidence: 0.75,
            }],
        );
        // Top-left border pixel of the box is set…
        assert_eq!(frame[20 * 64 + 50], BOX_LUMA);
        // …and nothing outside the luma plane was touched.
        assert!(frame[64 * 32..].iter().all(|&px| px == 0));
    }

    #[test]
    fn stale_boxes_expire() {
        let overlay = DetectionOverlay::new(100);
        overlay.update(&[crate::ai::engine::AiEvent {
            label: "x".into(),
            confidence: 0.9,
            x: 1,
            y: 1,
            w: 5,
            h: 5,
        }]);
        assert_eq!(overlay.active_boxes().len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(overlay.active_boxes().is_empty());
    }

    #[test]
    fn sanitize_strips_launch_breakers() {
        assert_eq!(sanitize("a\"b!c\\d"), "a b c d");
    }
}
