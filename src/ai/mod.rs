// src/ai/mod.rs
//
// The AI subsystem, layered so every industrial deployment shape is a config
// change, not a fork:
//
//   engine     orchestration (rate limit → preprocess → infer → parse)
//   runtime    InferenceRuntime trait + backend factory (`ai.runtime`)
//   backends/  one module per silicon runtime — onnx today; rknn, tensorrt,
//              openvino, hailo, tflite slot in behind the same trait
//   parser     OutputParser trait + architecture decoders (`ai.parser`)
//   preprocess camera-native pixel formats → letterboxed NCHW tensors
pub mod backends;
pub mod engine;
pub mod parser;
pub mod preprocess;
pub mod runtime;
