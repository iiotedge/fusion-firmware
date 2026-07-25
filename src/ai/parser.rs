// src/ai/parser.rs
//
// Model-output parsing, decoupled from the inference runtime: the same
// YOLOv8 parser works whether the tensors came from ONNX Runtime, RKNN or
// TensorRT. Adding an architecture (YOLOv5 layout, RT-DETR, classifiers,
// segmentation) = implement `OutputParser` here and register it in
// `create_parser`; selected via `ai.parser` in config.
use crate::ai::preprocess::Letterbox;
use crate::ai::runtime::OutputTensor;
use crate::core::error::{EdgeError, EdgeResult};

/// One detection in full-frame pixel coordinates.
#[derive(Debug, Clone)]
pub struct Detection {
    pub class_id: usize,
    pub label: String,
    pub confidence: f32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Everything a parser needs besides the raw tensors.
pub struct ParseContext<'a> {
    pub confidence_threshold: f32,
    pub nms_iou_threshold: f32,
    pub letterbox: &'a Letterbox,
    /// Class-id → human label. Missing ids become "class_<id>".
    pub labels: &'a [String],
    /// When non-empty, only these labels are reported.
    pub class_filter: &'a [String],
}

impl ParseContext<'_> {
    fn label_for(&self, class_id: usize) -> String {
        self.labels
            .get(class_id)
            .cloned()
            .unwrap_or_else(|| format!("class_{class_id}"))
    }

    fn passes_filter(&self, label: &str) -> bool {
        self.class_filter.is_empty() || self.class_filter.iter().any(|f| f == label)
    }
}

pub trait OutputParser: Send {
    fn name(&self) -> &'static str;
    fn parse(&self, outputs: &[OutputTensor], ctx: &ParseContext) -> EdgeResult<Vec<Detection>>;
}

/// Resolve the configured `ai.parser`.
pub fn create_parser(name: &str) -> EdgeResult<Box<dyn OutputParser>> {
    match name.to_lowercase().as_str() {
        // v8/v9/v11 share the anchor-free [1, 4+nc, N] head layout.
        "yolov8" | "yolov9" | "yolov11" | "yolo" => Ok(Box::new(YoloV8Parser)),
        other => Err(EdgeError::AiPanic(format!(
            "unknown ai.parser '{other}'; built-in: yolov8 (covers v9/v11) — \
             add architectures by implementing ai::parser::OutputParser"
        ))),
    }
}

/// Ultralytics anchor-free detection head: [1, 4+nc, anchors] (or the
/// transposed [1, anchors, 4+nc]) with cx,cy,w,h in input pixels followed by
/// nc class scores (no objectness).
pub struct YoloV8Parser;

impl OutputParser for YoloV8Parser {
    fn name(&self) -> &'static str {
        "yolov8"
    }

    fn parse(&self, outputs: &[OutputTensor], ctx: &ParseContext) -> EdgeResult<Vec<Detection>> {
        let out = outputs
            .iter()
            .find(|o| o.shape.len() == 3 && o.shape[0] == 1)
            .ok_or_else(|| {
                EdgeError::AiPanic(format!(
                    "no [1, A, N] detection tensor among outputs (shapes: {:?})",
                    outputs.iter().map(|o| &o.shape).collect::<Vec<_>>()
                ))
            })?;

        // Attributes (4+nc) is the smaller dimension in every practical
        // model: nc ≤ ~1000 classes vs ≥ 2100 anchors at 320px.
        let (d1, d2) = (out.shape[1] as usize, out.shape[2] as usize);
        let (attrs, anchors, attrs_first) = if d1 <= d2 {
            (d1, d2, true)
        } else {
            (d2, d1, false)
        };
        if attrs < 5 {
            return Err(EdgeError::AiPanic(format!(
                "detection tensor has {attrs} attributes; expected 4 box coords + ≥1 class"
            )));
        }
        let classes = attrs - 4;
        let at = |anchor: usize, attr: usize| -> f32 {
            if attrs_first {
                out.data[attr * anchors + anchor]
            } else {
                out.data[anchor * attrs + attr]
            }
        };

        let mut candidates = Vec::new();
        for a in 0..anchors {
            let mut best_class = 0usize;
            let mut best_score = 0f32;
            for c in 0..classes {
                let score = at(a, 4 + c);
                if score > best_score {
                    best_score = score;
                    best_class = c;
                }
            }
            if best_score < ctx.confidence_threshold {
                continue;
            }
            let label = ctx.label_for(best_class);
            if !ctx.passes_filter(&label) {
                continue;
            }
            let (x, y, w, h) = ctx
                .letterbox
                .frame_box(at(a, 0), at(a, 1), at(a, 2), at(a, 3));
            if w == 0 || h == 0 {
                continue;
            }
            candidates.push(Detection {
                class_id: best_class,
                label,
                confidence: best_score,
                x,
                y,
                w,
                h,
            });
        }

        Ok(non_max_suppression(candidates, ctx.nms_iou_threshold))
    }
}

/// Greedy per-class NMS: keep the highest-confidence box, drop same-class
/// boxes overlapping it beyond the IoU threshold.
pub fn non_max_suppression(mut detections: Vec<Detection>, iou_threshold: f32) -> Vec<Detection> {
    detections.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    let mut kept: Vec<Detection> = Vec::with_capacity(detections.len().min(64));
    for det in detections {
        let suppressed = kept
            .iter()
            .any(|k| k.class_id == det.class_id && iou(k, &det) > iou_threshold);
        if !suppressed {
            kept.push(det);
        }
    }
    kept
}

fn iou(a: &Detection, b: &Detection) -> f32 {
    let ax1 = a.x as f32;
    let ay1 = a.y as f32;
    let ax2 = ax1 + a.w as f32;
    let ay2 = ay1 + a.h as f32;
    let bx1 = b.x as f32;
    let by1 = b.y as f32;
    let bx2 = bx1 + b.w as f32;
    let by2 = by1 + b.h as f32;

    let iw = (ax2.min(bx2) - ax1.max(bx1)).max(0.0);
    let ih = (ay2.min(by2) - ay1.max(by1)).max(0.0);
    let inter = iw * ih;
    let union = (a.w as f32 * a.h as f32) + (b.w as f32 * b.h as f32) - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_letterbox(w: u32, h: u32) -> Letterbox {
        Letterbox {
            scale: 1.0,
            pad_x: 0.0,
            pad_y: 0.0,
            roi_x: 0,
            roi_y: 0,
            frame_w: w,
            frame_h: h,
        }
    }

    /// Build a [1, 4+nc, anchors] attrs-first tensor from anchor rows.
    fn tensor(anchor_rows: &[Vec<f32>]) -> OutputTensor {
        let anchors = anchor_rows.len();
        let attrs = anchor_rows[0].len();
        let mut data = vec![0f32; attrs * anchors];
        for (a, row) in anchor_rows.iter().enumerate() {
            for (attr, value) in row.iter().enumerate() {
                data[attr * anchors + a] = *value;
            }
        }
        OutputTensor {
            data,
            shape: vec![1, attrs as i64, anchors as i64],
        }
    }

    #[test]
    fn decodes_confident_box_and_drops_weak_one() {
        // 2 classes: anchor 0 = strong class 1, anchor 1 = below threshold,
        // plus empty anchors so anchors > attrs like every real export.
        let mut rows = vec![
            vec![100.0, 100.0, 40.0, 20.0, 0.05, 0.9],
            vec![300.0, 300.0, 40.0, 40.0, 0.2, 0.1],
        ];
        rows.extend(std::iter::repeat_n(vec![0.0; 6], 8));
        let out = tensor(&rows);
        let lb = identity_letterbox(640, 640);
        let labels = vec!["helmet".to_string(), "no_helmet".to_string()];
        let ctx = ParseContext {
            confidence_threshold: 0.5,
            nms_iou_threshold: 0.45,
            letterbox: &lb,
            labels: &labels,
            class_filter: &[],
        };
        let dets = YoloV8Parser.parse(&[out], &ctx).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].label, "no_helmet");
        assert_eq!(
            (dets[0].x, dets[0].y, dets[0].w, dets[0].h),
            (80, 90, 40, 20)
        );
    }

    #[test]
    fn nms_suppresses_overlapping_same_class() {
        let mk = |x, conf| Detection {
            class_id: 0,
            label: "obj".into(),
            confidence: conf,
            x,
            y: 10,
            w: 100,
            h: 100,
        };
        let kept = non_max_suppression(vec![mk(10, 0.8), mk(20, 0.9), mk(400, 0.7)], 0.45);
        assert_eq!(kept.len(), 2);
        assert!((kept[0].confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn class_filter_limits_labels() {
        let mut rows = vec![vec![50.0, 50.0, 20.0, 20.0, 0.9]];
        rows.extend(std::iter::repeat_n(vec![0.0; 5], 8));
        let out = tensor(&rows);
        let lb = identity_letterbox(640, 640);
        let labels = vec!["forklift".to_string()];
        let ctx = ParseContext {
            confidence_threshold: 0.5,
            nms_iou_threshold: 0.45,
            letterbox: &lb,
            labels: &labels,
            class_filter: &["person".to_string()],
        };
        assert!(YoloV8Parser.parse(&[out], &ctx).unwrap().is_empty());
    }

    #[test]
    fn handles_transposed_layout() {
        // [1, anchors, attrs] with anchors > attrs — anchors-first export.
        let out = OutputTensor {
            data: vec![
                100.0, 100.0, 40.0, 20.0, 0.9, // anchor 0
                10.0, 10.0, 4.0, 4.0, 0.1, // anchor 1
                10.0, 10.0, 4.0, 4.0, 0.1, // anchor 2
                10.0, 10.0, 4.0, 4.0, 0.1, // anchor 3
                10.0, 10.0, 4.0, 4.0, 0.1, // anchor 4
                10.0, 10.0, 4.0, 4.0, 0.1, // anchor 5
            ],
            shape: vec![1, 6, 5],
        };
        let lb = identity_letterbox(640, 640);
        let ctx = ParseContext {
            confidence_threshold: 0.5,
            nms_iou_threshold: 0.45,
            letterbox: &lb,
            labels: &[],
            class_filter: &[],
        };
        let dets = YoloV8Parser.parse(&[out], &ctx).unwrap();
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].label, "class_0");
    }
}
