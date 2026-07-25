// src/ai/engine.rs
//
// The inference orchestrator: rate-limit → preprocess → runtime.infer →
// parse → events. Contains zero knowledge of model architecture or silicon —
// those live behind the OutputParser and InferenceRuntime traits, both
// selected purely by config, so the same firmware serves a YOLO-on-CPU
// Radxa node and (once those backends land) a TensorRT Jetson or an
// RKNN/Hailo box without code changes.
use crate::ai::parser::{create_parser, OutputParser, ParseContext};
use crate::ai::preprocess::Preprocessor;
use crate::ai::runtime::{create_runtime, InferenceRuntime};
use crate::config::{AiConfig, CameraConfig};
use crate::core::error::EdgeResult;
use crate::hal::FrameHandle;

use std::time::{Duration, Instant};
use tracing::{debug, info};

/// One reportable detection, in full-frame pixel coordinates. Serialized to
/// telemetry (GDE JSON) and consumed by overlays (Phase 4) and event clips
/// (Phase 5).
#[derive(Debug)]
pub struct AiEvent {
    pub label: String,
    pub confidence: f32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

pub struct InferenceEngine {
    runtime: Box<dyn InferenceRuntime>,
    parser: Box<dyn OutputParser>,
    preprocessor: Preprocessor,
    labels: Vec<String>,
    class_filter: Vec<String>,
    confidence_threshold: f32,
    nms_iou_threshold: f32,
    /// Minimum wall-clock spacing between inferences (None = every frame).
    min_interval: Option<Duration>,
    last_inference: Option<Instant>,
}

impl InferenceEngine {
    pub fn new(ai: &AiConfig, camera: &CameraConfig) -> EdgeResult<Self> {
        let preprocessor = Preprocessor::new(camera, ai)?;
        let runtime = create_runtime(ai)?;
        let parser = create_parser(&ai.parser)?;

        let min_interval = (ai.inference_fps_limit > 0)
            .then(|| Duration::from_secs_f64(1.0 / f64::from(ai.inference_fps_limit)));

        info!(
            runtime = runtime.name(),
            parser = parser.name(),
            input = format!("{}x{}", ai.input_width, ai.input_height),
            fps_limit = ai.inference_fps_limit,
            classes = ai.labels.len(),
            "AI inference engine ready"
        );

        Ok(Self {
            runtime,
            parser,
            preprocessor,
            labels: ai.labels.clone(),
            class_filter: ai.class_filter.clone(),
            confidence_threshold: ai.confidence_threshold,
            nms_iou_threshold: ai.nms_iou_threshold,
            min_interval,
            last_inference: None,
        })
    }

    /// Bypasses the fps-limit skip for the very next `run_inference` call
    /// only, then reverts to normal spacing — used for peer-triggered
    /// re-analysis (the `reanalyze` command, see commands.rs): a peer's
    /// detection means this device's own view is worth checking right now,
    /// not whenever the fps limiter would otherwise allow it.
    pub fn force_next(&mut self) {
        self.last_inference = None;
    }

    /// Run inference on one frame. Returns an empty list when the frame is
    /// skipped by the fps limiter — skipping is flow control, not an error.
    pub fn run_inference(&mut self, frame: &FrameHandle) -> EdgeResult<Vec<AiEvent>> {
        if let (Some(interval), Some(last)) = (self.min_interval, self.last_inference) {
            if last.elapsed() < interval {
                return Ok(Vec::new());
            }
        }
        self.last_inference = Some(Instant::now());

        // SAFETY: the HAL guarantees data_ptr/size describe a live frame for
        // the duration of this call (see FrameHandle contract).
        let pixels = unsafe { std::slice::from_raw_parts(frame.data_ptr, frame.size) };

        let started = Instant::now();
        let (tensor, letterbox) = self.preprocessor.run(pixels)?;
        let outputs = self.runtime.infer(tensor)?;

        let ctx = ParseContext {
            confidence_threshold: self.confidence_threshold,
            nms_iou_threshold: self.nms_iou_threshold,
            letterbox: &letterbox,
            labels: &self.labels,
            class_filter: &self.class_filter,
        };
        let detections = self.parser.parse(&outputs, &ctx)?;

        debug!(
            frame = frame.id,
            latency_ms = started.elapsed().as_millis() as u64,
            detections = detections.len(),
            "inference complete"
        );

        Ok(detections
            .into_iter()
            .map(|d| AiEvent {
                label: d.label,
                confidence: d.confidence,
                x: d.x,
                y: d.y,
                w: d.w,
                h: d.h,
            })
            .collect())
    }
}
