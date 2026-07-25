// src/stream/widgets.rs
//
// Realtime machine-data widgets burned into the video (F6 "HMI-on-video"):
// sparkline trends, bar gauges and live values of southbound tags — the
// Industry 4.0 operator view where the process data is *on* the footage,
// live and in recordings alike.
//
//   machine event (serial/CAN/Modbus…) ─ lib ingest path ─► WidgetFeed
//   (Processor) ─► OverlayDataBus ring buffers ─► media thread renders
//   config-declared widgets into the luma plane each frame.
//
// Rendering is dependency-free: a built-in 5×7 bitmap font (digits, A–Z and
// units punctuation) and line/box primitives on the shared FrameLayout.
use crate::config::WidgetConfig;
use crate::stream::overlay::FrameLayout;

use iiotedge_core::traits::Processor;
use iiotedge_core::types::UnifiedPayload;
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Bright/dim luma values for widget strokes and backgrounds.
const STROKE: u8 = 235;
const BACKDROP: u8 = 32;
/// Series ring capacity — bounds memory regardless of window_s.
const MAX_POINTS: usize = 600;

// ---------------------------------------------------------------------------
// Data bus
// ---------------------------------------------------------------------------

/// Per-source time series shared between the telemetry tap (writer) and the
/// media thread (reader).
#[derive(Default)]
pub struct OverlayDataBus {
    series: Mutex<HashMap<String, VecDeque<(Instant, f64)>>>,
}

impl OverlayDataBus {
    pub fn push(&self, source: &str, value: f64) {
        let mut series = self.series.lock();
        let ring = series.entry(source.to_string()).or_default();
        ring.push_back((Instant::now(), value));
        if ring.len() > MAX_POINTS {
            ring.pop_front();
        }
    }

    /// Points newer than `window`, oldest first; empty if the source is quiet.
    pub fn window(&self, source: &str, window: Duration) -> Vec<(Instant, f64)> {
        let series = self.series.lock();
        let Some(ring) = series.get(source) else {
            return Vec::new();
        };
        let cutoff = Instant::now() - window;
        ring.iter().filter(|(t, _)| *t >= cutoff).copied().collect()
    }

    pub fn latest(&self, source: &str) -> Option<f64> {
        self.series.lock().get(source)?.back().map(|(_, v)| *v)
    }
}

// ---------------------------------------------------------------------------
// Telemetry-side feed (lib Processor)
// ---------------------------------------------------------------------------

/// Ingest-path tap: extracts numeric values for the configured widget
/// sources. Cheap by contract — prefix match + number parse + ring push.
pub struct WidgetFeed {
    bindings: Vec<(String, String)>, // (source_prefix, json_field)
    bus: Arc<OverlayDataBus>,
}

impl WidgetFeed {
    /// None when no widgets are configured (inert).
    pub fn new(widgets: &[WidgetConfig]) -> Option<(Arc<Self>, Arc<OverlayDataBus>)> {
        if widgets.is_empty() {
            return None;
        }
        let bus = Arc::new(OverlayDataBus::default());
        let bindings = widgets
            .iter()
            .map(|w| (w.source.clone(), w.json_field.clone()))
            .collect();
        info!(widgets = widgets.len(), "On-video machine widgets active");
        Some((
            Arc::new(Self {
                bindings,
                bus: bus.clone(),
            }),
            bus,
        ))
    }
}

impl Processor for WidgetFeed {
    fn process(&self, payload: UnifiedPayload) -> Option<UnifiedPayload> {
        for (prefix, json_field) in &self.bindings {
            if payload.source_id.starts_with(prefix.as_str()) {
                if let Some(value) = extract_value(&payload.payload, json_field) {
                    // Key by source AND field: two widgets reading different
                    // fields of the same source must not share a series.
                    self.bus.push(&series_key(prefix, json_field), value);
                    debug!(source = %prefix, field = %json_field, value, "widget datapoint");
                }
            }
        }
        Some(payload)
    }
}

/// Series identifier: source prefix + extracted field.
fn series_key(source: &str, json_field: &str) -> String {
    format!("{source}#{json_field}")
}

fn widget_key(w: &WidgetConfig) -> String {
    series_key(&w.source, &w.json_field)
}

/// Payload → number: JSON field lookup when configured, else the payload
/// parsed as a plain number.
fn extract_value(payload: &[u8], json_field: &str) -> Option<f64> {
    if json_field.is_empty() {
        return std::str::from_utf8(payload).ok()?.trim().parse().ok();
    }
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    value.get(json_field)?.as_f64()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Draw all configured widgets into the frame's luma plane.
pub fn draw_widgets(
    frame: &mut [u8],
    layout: FrameLayout,
    widgets: &[WidgetConfig],
    bus: &OverlayDataBus,
) {
    for widget in widgets {
        match widget.kind.as_str() {
            "sparkline" => draw_sparkline(frame, layout, widget, bus),
            "gauge" => draw_gauge(frame, layout, widget, bus),
            _ => draw_value(frame, layout, widget, bus),
        }
    }
}

fn header_text(widget: &WidgetConfig, value: Option<f64>) -> String {
    let value_text = match value {
        Some(v) if v.abs() >= 100.0 => format!("{v:.0}"),
        Some(v) => format!("{v:.1}"),
        None => "--".to_string(),
    };
    if widget.label.is_empty() {
        value_text
    } else {
        format!("{} {}", widget.label, value_text)
    }
}

fn draw_sparkline(frame: &mut [u8], layout: FrameLayout, w: &WidgetConfig, bus: &OverlayDataBus) {
    let points = bus.window(&widget_key(w), Duration::from_secs(w.window_s.max(1)));
    backdrop(frame, layout, w.x, w.y, w.width, w.height);
    rect(frame, layout, w.x, w.y, w.width, w.height);
    let header = header_text(w, points.last().map(|(_, v)| *v));
    draw_text(frame, layout, w.x + 6, w.y + 4, &header, 2);

    let plot_top = w.y + 22;
    let plot_h = w.height.saturating_sub(28);
    let plot_w = w.width.saturating_sub(12);
    if points.len() < 2 || plot_h < 8 || plot_w < 8 {
        return;
    }

    // Scale: config min/max, widened to the data when they collapse.
    let (mut lo, mut hi) = (w.min, w.max);
    if hi <= lo {
        lo = points.iter().map(|(_, v)| *v).fold(f64::MAX, f64::min);
        hi = points.iter().map(|(_, v)| *v).fold(f64::MIN, f64::max);
        if hi <= lo {
            hi = lo + 1.0;
        }
    }

    let n = points.len();
    let mut previous: Option<(u32, u32)> = None;
    for (index, (_, value)) in points.iter().enumerate() {
        let px = w.x + 6 + (index as u32 * (plot_w - 1) / (n as u32 - 1).max(1));
        let norm = ((value - lo) / (hi - lo)).clamp(0.0, 1.0);
        let py = plot_top + plot_h - 1 - (norm * f64::from(plot_h - 1)) as u32;
        if let Some((ax, ay)) = previous {
            line(frame, layout, ax, ay, px, py);
        }
        previous = Some((px, py));
    }
}

fn draw_gauge(frame: &mut [u8], layout: FrameLayout, w: &WidgetConfig, bus: &OverlayDataBus) {
    let value = bus.latest(&widget_key(w));
    backdrop(frame, layout, w.x, w.y, w.width, w.height);
    rect(frame, layout, w.x, w.y, w.width, w.height);
    draw_text(frame, layout, w.x + 6, w.y + 4, &header_text(w, value), 2);

    let bar_y = w.y + 24;
    let bar_h = w.height.saturating_sub(30);
    let bar_w = w.width.saturating_sub(12);
    if bar_h < 6 || bar_w < 6 {
        return;
    }
    rect(frame, layout, w.x + 6, bar_y, bar_w, bar_h);
    if let Some(v) = value {
        let span = (w.max - w.min).abs().max(f64::EPSILON);
        let norm = ((v - w.min) / span).clamp(0.0, 1.0);
        let fill = (norm * f64::from(bar_w - 4)) as u32;
        for row in 2..bar_h.saturating_sub(2) {
            for col in 0..fill {
                set(frame, layout, w.x + 8 + col, bar_y + row);
            }
        }
    }
}

fn draw_value(frame: &mut [u8], layout: FrameLayout, w: &WidgetConfig, bus: &OverlayDataBus) {
    let value = bus.latest(&widget_key(w));
    backdrop(frame, layout, w.x, w.y, w.width, w.height);
    rect(frame, layout, w.x, w.y, w.width, w.height);
    if !w.label.is_empty() {
        draw_text(frame, layout, w.x + 6, w.y + 4, &w.label, 2);
    }
    let value_text = match value {
        Some(v) if v.abs() >= 1000.0 => format!("{v:.0}"),
        Some(v) => format!("{v:.1}"),
        None => "--".to_string(),
    };
    draw_text(frame, layout, w.x + 6, w.y + 22, &value_text, 3);
}

// --- primitives -------------------------------------------------------------

#[inline]
fn set(frame: &mut [u8], layout: FrameLayout, x: u32, y: u32) {
    layout.set_luma_value(frame, x as usize, y as usize, STROKE);
}

pub(crate) fn backdrop(frame: &mut [u8], layout: FrameLayout, x: u32, y: u32, w: u32, h: u32) {
    for row in 0..h {
        for col in 0..w {
            layout.set_luma_value(frame, (x + col) as usize, (y + row) as usize, BACKDROP);
        }
    }
}

fn rect(frame: &mut [u8], layout: FrameLayout, x: u32, y: u32, w: u32, h: u32) {
    if w == 0 || h == 0 {
        return;
    }
    for col in 0..w {
        set(frame, layout, x + col, y);
        set(frame, layout, x + col, y + h - 1);
    }
    for row in 0..h {
        set(frame, layout, x, y + row);
        set(frame, layout, x + w - 1, y + row);
    }
}

/// Bresenham line, luma stroke.
fn line(frame: &mut [u8], layout: FrameLayout, x0: u32, y0: u32, x1: u32, y1: u32) {
    let (mut x0, mut y0) = (x0 as i64, y0 as i64);
    let (x1, y1) = (x1 as i64, y1 as i64);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if x0 >= 0 && y0 >= 0 {
            set(frame, layout, x0 as u32, y0 as u32);
            set(frame, layout, x0 as u32, y0 as u32 + 1); // 2px stroke
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

// --- 5×7 bitmap font ---------------------------------------------------------

/// Render ASCII text (digits, A–Z, and units punctuation) at pixel scale.
pub fn draw_text(frame: &mut [u8], layout: FrameLayout, x: u32, y: u32, text: &str, scale: u32) {
    let mut cursor = x;
    for ch in text.to_ascii_uppercase().chars() {
        let glyph = glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (0b10000 >> col) != 0 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            set(
                                frame,
                                layout,
                                cursor + col * scale + sx,
                                y + row as u32 * scale + sy,
                            );
                        }
                    }
                }
            }
        }
        cursor += 6 * scale;
    }
}

/// 5×7 glyphs, one byte per row, low 5 bits used.
fn glyph(ch: char) -> [u8; 7] {
    match ch {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        _ => [0x00; 7], // space and anything unmapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_and_json_values() {
        assert_eq!(extract_value(b" 42.5 ", ""), Some(42.5));
        assert_eq!(extract_value(br#"{"c": 21.5, "f": 70}"#, "c"), Some(21.5));
        assert_eq!(extract_value(br#"{"c": "hot"}"#, "c"), None);
        assert_eq!(extract_value(b"not a number", ""), None);
    }

    #[test]
    fn bus_window_and_latest() {
        let bus = OverlayDataBus::default();
        bus.push("modbus/plc1/temp", 20.0);
        bus.push("modbus/plc1/temp", 21.0);
        assert_eq!(bus.latest("modbus/plc1/temp"), Some(21.0));
        assert_eq!(
            bus.window("modbus/plc1/temp", Duration::from_secs(60))
                .len(),
            2
        );
        assert!(bus.latest("unknown").is_none());
    }

    #[test]
    fn text_renders_into_luma() {
        let layout = FrameLayout::Nv12 {
            width: 128,
            height: 64,
        };
        let mut frame = vec![0u8; 128 * 64 * 3 / 2];
        draw_text(&mut frame, layout, 2, 2, "A1.", 1);
        let luma = &frame[..128 * 64];
        assert!(luma.contains(&STROKE), "glyph pixels set");
        // Chroma untouched.
        assert!(frame[128 * 64..].iter().all(|&px| px == 0));
    }
}
