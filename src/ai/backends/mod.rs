// src/ai/backends/mod.rs
//
// One module per inference silicon/runtime. Each implements
// ai::runtime::InferenceRuntime and is selected via `ai.runtime` in config.
// Vendor SDK linkage (librknnrt, TensorRT, OpenVINO, HailoRT) belongs behind
// cargo features on the corresponding module — never in shared code.
pub mod onnx;
// Rockchip NPU: dlopens librknnrt.so at runtime, so it compiles into every
// Linux build without link-time vendor coupling.
#[cfg(target_os = "linux")]
pub mod rknn;
