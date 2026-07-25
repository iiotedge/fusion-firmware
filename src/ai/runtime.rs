// src/ai/runtime.rs
//
// The runtime-agnostic inference boundary. Industrial deployments span ONNX
// Runtime (x86 box PCs, ARM CPU), Rockchip RKNN, NVIDIA TensorRT, Intel
// OpenVINO, HailoRT and LiteRT — one firmware must swap between them by
// **configuration only**. Everything above this boundary (preprocessing,
// parsers, event fan-out) is engine-independent; everything below it lives
// in exactly one backend module under ai/backends/.
//
// Adding a backend = implement `InferenceRuntime` in src/ai/backends/<name>.rs
// (optionally behind a cargo feature for vendor SDK linkage) and add one arm
// in `create_runtime`. Nothing else in the firmware changes.
use crate::config::AiConfig;
use crate::core::error::{EdgeError, EdgeResult};

/// Normalized input: NCHW f32 in [0,1]. Backends needing other layouts or
/// quantization (RKNN INT8, LiteRT NHWC) convert internally — keeping the
/// application layer identical across silicon.
pub struct InputTensor {
    pub data: Vec<f32>,
    /// [batch, channels, height, width]
    pub shape: [i64; 4],
}

/// One raw output tensor, in the model's declared output order.
pub struct OutputTensor {
    pub data: Vec<f32>,
    pub shape: Vec<i64>,
}

pub trait InferenceRuntime: Send {
    /// Backend identifier for logs/telemetry (e.g. "onnxruntime").
    fn name(&self) -> &'static str;

    /// Execute one inference on a single frame's tensor.
    fn infer(&mut self, input: InputTensor) -> EdgeResult<Vec<OutputTensor>>;
}

/// Backends on the integration roadmap: recognized in config with a precise
/// error, so a fleet config written for tomorrow's build fails with guidance
/// rather than "unknown runtime".
const PLANNED_RUNTIMES: &[&str] = &["tensorrt", "openvino", "hailo", "tflite", "litert"];

/// Resolve the configured `ai.runtime` to a concrete backend.
pub fn create_runtime(cfg: &AiConfig) -> EdgeResult<Box<dyn InferenceRuntime>> {
    let runtime = cfg.runtime.to_lowercase();
    match runtime.as_str() {
        "onnx" | "onnxruntime" | "ort" => {
            Ok(Box::new(super::backends::onnx::OnnxRuntime::new(cfg)?))
        }
        "rknn" => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(super::backends::rknn::RknnRuntime::new(cfg)?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(EdgeError::AiPanic(
                    "ai.runtime 'rknn' targets Rockchip NPUs and is Linux-only; \
                     use ai.runtime = \"onnx\" on dev hosts"
                        .into(),
                ))
            }
        }
        planned if PLANNED_RUNTIMES.contains(&planned) => Err(EdgeError::AiPanic(format!(
            "ai.runtime '{planned}' is on the roadmap but not compiled into this build; \
             implement ai::runtime::InferenceRuntime in src/ai/backends/{planned}.rs and \
             register it in create_runtime. Until then set ai.runtime = \"onnx\"."
        ))),
        other => Err(EdgeError::AiPanic(format!(
            "unknown ai.runtime '{other}'; built-in: onnx, rknn (Linux) — planned: {}",
            PLANNED_RUNTIMES.join(", ")
        ))),
    }
}
