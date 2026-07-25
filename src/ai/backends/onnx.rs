// src/ai/backends/onnx.rs
//
// ONNX Runtime backend — the universal baseline for industrial vision
// (x86 box PCs, ARM CPU; bridges to CUDA/TensorRT/OpenVINO/CoreML via
// execution-provider cargo features when a fleet needs them).
use crate::ai::runtime::{InferenceRuntime, InputTensor, OutputTensor};
use crate::config::AiConfig;
use crate::core::error::{EdgeError, EdgeResult};

use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

pub struct OnnxRuntime {
    session: Session,
    input_name: String,
    output_names: Vec<String>,
}

impl OnnxRuntime {
    pub fn new(cfg: &AiConfig) -> EdgeResult<Self> {
        // Linux builds load ONNX Runtime at runtime (load-dynamic) — resolve
        // and dlopen it *before* any other ort call, because the lazy API
        // setup panics on a missing dylib while init_from returns an error we
        // can degrade on gracefully.
        #[cfg(target_os = "linux")]
        init_dynamic_runtime(cfg)?;

        // Read once: fingerprint + load from the same bytes, so the logged
        // identity is exactly what runs (fleet-auditable model provenance).
        let model_bytes = std::fs::read(&cfg.model_path)
            .map_err(|e| EdgeError::AiPanic(format!("read model {}: {e}", cfg.model_path)))?;
        let sha256 = hex(&Sha256::digest(&model_bytes));

        let builder =
            Session::builder().map_err(|e| EdgeError::AiPanic(format!("session builder: {e}")))?;
        let builder = builder
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| EdgeError::AiPanic(format!("optimization level: {e}")))?;
        let builder = builder
            .with_intra_threads(cfg.intra_threads.max(1))
            .map_err(|e| EdgeError::AiPanic(format!("intra threads: {e}")))?;
        let mut builder = apply_delegate(builder, &cfg.hardware_delegate);

        let session = builder
            .commit_from_memory(&model_bytes)
            .map_err(|e| EdgeError::AiPanic(format!("load model {}: {e}", cfg.model_path)))?;

        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| EdgeError::AiPanic("model declares no inputs".into()))?;
        let output_names: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        if output_names.is_empty() {
            return Err(EdgeError::AiPanic("model declares no outputs".into()));
        }

        info!(
            model = %cfg.model_path,
            sha256 = %sha256,
            size_bytes = model_bytes.len(),
            input = %input_name,
            outputs = ?output_names,
            "ONNX Runtime session ready"
        );

        Ok(Self {
            session,
            input_name,
            output_names,
        })
    }
}

/// dlopen libonnxruntime.so exactly once per process. Resolution order:
/// `ai.onnx_dylib_path` → `ORT_DYLIB_PATH` env → the loader's default search
/// for "libonnxruntime.so".
#[cfg(target_os = "linux")]
fn init_dynamic_runtime(cfg: &AiConfig) -> EdgeResult<()> {
    use std::sync::OnceLock;
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();

    let outcome = INIT.get_or_init(|| {
        let path = if !cfg.onnx_dylib_path.trim().is_empty() {
            cfg.onnx_dylib_path.trim().to_string()
        } else if let Ok(env_path) = std::env::var("ORT_DYLIB_PATH") {
            if env_path.is_empty() {
                "libonnxruntime.so".to_string()
            } else {
                env_path
            }
        } else {
            "libonnxruntime.so".to_string()
        };
        match ort::init_from(&path) {
            Ok(builder) => {
                builder.commit();
                info!("Loaded ONNX Runtime dynamic library: {path}");
                Ok(())
            }
            Err(e) => Err(format!(
                "cannot load ONNX Runtime dylib '{path}': {e}. Install onnxruntime on this \
                 device (or set ai.onnx_dylib_path), or use ai.runtime = \"rknn\" on Rockchip"
            )),
        }
    });
    outcome.clone().map_err(EdgeError::AiPanic)
}

/// Map the advisory `ai.hardware_delegate` onto ONNX Runtime execution
/// providers. This build compiles the CPU provider only; accelerator EPs are
/// opt-in cargo features of `ort` (cuda, tensorrt, openvino, coreml, …) —
/// enable the feature and extend this match, nothing else changes.
fn apply_delegate(
    builder: ort::session::builder::SessionBuilder,
    delegate: &str,
) -> ort::session::builder::SessionBuilder {
    match delegate.to_uppercase().as_str() {
        "" | "CPU" | "AUTO" => {
            info!("ONNX Runtime executing on CPU");
            builder
        }
        other => {
            warn!(
                "ai.hardware_delegate '{other}' has no execution provider compiled into this \
                 build; running on CPU. Enable the matching ort cargo feature (cuda / tensorrt / \
                 openvino / coreml) and extend ai::backends::onnx::apply_delegate — or use a \
                 dedicated runtime backend (ai.runtime = rknn/hailo/…) once integrated."
            );
            builder
        }
    }
}

impl InferenceRuntime for OnnxRuntime {
    fn name(&self) -> &'static str {
        "onnxruntime"
    }

    fn infer(&mut self, input: InputTensor) -> EdgeResult<Vec<OutputTensor>> {
        let tensor = ort::value::Tensor::from_array((input.shape.to_vec(), input.data))
            .map_err(|e| EdgeError::AiPanic(format!("input tensor: {e}")))?;

        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .map_err(|e| EdgeError::AiPanic(format!("inference: {e}")))?;

        let mut result = Vec::with_capacity(self.output_names.len());
        for name in &self.output_names {
            let value = outputs.get(name).ok_or_else(|| {
                EdgeError::AiPanic(format!("model output '{name}' missing from results"))
            })?;
            let (shape, data) = value.try_extract_tensor::<f32>().map_err(|e| {
                EdgeError::AiPanic(format!(
                    "output '{name}' is not an f32 tensor ({e}); quantized-output models need a \
                     dequantizing parser or a backend-native runtime"
                ))
            })?;
            result.push(OutputTensor {
                data: data.to_vec(),
                shape: shape.to_vec(),
            });
        }
        Ok(result)
    }
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
