// src/ai/backends/rknn.rs
//
// Rockchip RKNN NPU backend (RK3566/RK3568/RK3588 — RKNPU2 runtime).
//
// Design: `librknnrt.so` is dlopen'd at runtime instead of linked — the same
// firmware binary serves NPU-less fleets, and a Rockchip device without the
// runtime installed gets a precise "install librknnrt" error rather than a
// binary that cannot start. Inputs are fed as FLOAT32/NHWC with
// `pass_through = 0` and outputs fetched with `want_float = 1`, so the
// runtime performs quantize/dequantize internally and this backend slots
// behind the same f32 InferenceRuntime boundary as ONNX. NHWC is not
// optional: on physical RK3566 hardware this RKNPU2 runtime's input
// normalize step rejects NCHW outright ("Meet unsupported src layout for
// normalize: NCHW, only support NHWC src layout!", confirmed on-device
// 2026-07-19) — `infer()` converts preprocess.rs's NCHW-planar tensor to
// NHWC-interleaved before every call.
//
// Models: `.rknn` files converted offline with rknn-toolkit2 from the SAME
// ONNX export you would run on the onnx backend — detection-head layout is
// preserved, so the yolov8 parser applies unchanged.
//
// Status: fleet-deployable; verified against a physical RK3566 NPU
// (RKNPU2 runtime ≥ 1.3) on 2026-07-19.
#![allow(dead_code)] // FFI mirror structs carry fields we don't read (ABI layout)

use crate::ai::runtime::{InferenceRuntime, InputTensor, OutputTensor};
use crate::config::AiConfig;
use crate::core::error::{EdgeError, EdgeResult};

use libloading::{Library, Symbol};
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::ptr;
use tracing::{info, warn};

const RKNN_SUCC: i32 = 0;
const RKNN_MAX_DIMS: usize = 16;
const RKNN_MAX_NAME_LEN: usize = 256;

// rknn_query_cmd
const RKNN_QUERY_IN_OUT_NUM: u32 = 0;
const RKNN_QUERY_OUTPUT_ATTR: u32 = 2;
const RKNN_QUERY_SDK_VERSION: u32 = 5;

// rknn_tensor_type / rknn_tensor_format
const RKNN_TENSOR_FLOAT32: u32 = 0;
const RKNN_TENSOR_NCHW: u32 = 0;
const RKNN_TENSOR_NHWC: u32 = 1;

#[repr(C)]
struct RknnInputOutputNum {
    n_input: u32,
    n_output: u32,
}

#[repr(C)]
struct RknnSdkVersion {
    api_version: [u8; 256],
    drv_version: [u8; 256],
}

#[repr(C)]
struct RknnTensorAttr {
    index: u32,
    n_dims: u32,
    dims: [u32; RKNN_MAX_DIMS],
    name: [u8; RKNN_MAX_NAME_LEN],
    n_elems: u32,
    size: u32,
    fmt: u32,
    ttype: u32,
    qnt_type: u32,
    fl: i8,
    zp: i32,
    scale: f32,
    w_stride: u32,
    size_with_stride: u32,
    pass_through: u8,
    h_stride: u32,
}

impl Default for RknnTensorAttr {
    fn default() -> Self {
        // SAFETY: plain-old-data struct; zeroed is a valid initial value.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
struct RknnInput {
    index: u32,
    buf: *mut c_void,
    size: u32,
    pass_through: u8,
    ttype: u32,
    fmt: u32,
}

#[repr(C)]
struct RknnOutput {
    want_float: u8,
    is_prealloc: u8,
    index: u32,
    buf: *mut c_void,
    size: u32,
}

type RknnContext = u64;

type RknnInitFn = unsafe extern "C" fn(*mut RknnContext, *mut c_void, u32, u32, *mut c_void) -> i32;
type RknnDestroyFn = unsafe extern "C" fn(RknnContext) -> i32;
type RknnQueryFn = unsafe extern "C" fn(RknnContext, u32, *mut c_void, u32) -> i32;
type RknnInputsSetFn = unsafe extern "C" fn(RknnContext, u32, *mut RknnInput) -> i32;
type RknnRunFn = unsafe extern "C" fn(RknnContext, *mut c_void) -> i32;
type RknnOutputsGetFn = unsafe extern "C" fn(RknnContext, u32, *mut RknnOutput, *mut c_void) -> i32;
type RknnOutputsReleaseFn = unsafe extern "C" fn(RknnContext, u32, *mut RknnOutput) -> i32;

struct RknnApi {
    init: libloading::os::unix::Symbol<RknnInitFn>,
    destroy: libloading::os::unix::Symbol<RknnDestroyFn>,
    query: libloading::os::unix::Symbol<RknnQueryFn>,
    inputs_set: libloading::os::unix::Symbol<RknnInputsSetFn>,
    run: libloading::os::unix::Symbol<RknnRunFn>,
    outputs_get: libloading::os::unix::Symbol<RknnOutputsGetFn>,
    outputs_release: libloading::os::unix::Symbol<RknnOutputsReleaseFn>,
}

pub struct RknnRuntime {
    // Declaration order = drop order: context is destroyed in Drop before
    // the library handle is unloaded.
    ctx: RknnContext,
    api: RknnApi,
    _lib: Library,
    n_outputs: u32,
    output_shapes: Vec<Vec<i64>>,
    // Reused across infer() calls for the NCHW->NHWC conversion: every
    // element gets overwritten every call (see nchw_to_nhwc), so there's no
    // correctness reason to allocate+zero-fill a fresh ~4.7MB (640x640x3
    // f32) buffer 10x/sec on the analytics thread's real-time path.
    nhwc_scratch: Vec<f32>,
    // Needed to de-normalize this model's box output — see the comment on
    // the scaling loop in infer() below.
    input_width: f32,
    input_height: f32,
}

// SAFETY: the runtime is only driven from the analytics thread; RKNN
// contexts are usable from the creating process, and we never share &mut.
unsafe impl Send for RknnRuntime {}

impl RknnRuntime {
    pub fn new(cfg: &AiConfig) -> EdgeResult<Self> {
        let lib = load_runtime_library()?;

        // SAFETY: symbol names/signatures mirror rknn_api.h (RKNPU2).
        let api = unsafe {
            RknnApi {
                init: get::<RknnInitFn>(&lib, b"rknn_init")?,
                destroy: get::<RknnDestroyFn>(&lib, b"rknn_destroy")?,
                query: get::<RknnQueryFn>(&lib, b"rknn_query")?,
                inputs_set: get::<RknnInputsSetFn>(&lib, b"rknn_inputs_set")?,
                run: get::<RknnRunFn>(&lib, b"rknn_run")?,
                outputs_get: get::<RknnOutputsGetFn>(&lib, b"rknn_outputs_get")?,
                outputs_release: get::<RknnOutputsReleaseFn>(&lib, b"rknn_outputs_release")?,
            }
        };

        let mut model = std::fs::read(&cfg.model_path)
            .map_err(|e| EdgeError::AiPanic(format!("read model {}: {e}", cfg.model_path)))?;
        let sha256 = hex(&Sha256::digest(&model));

        let mut ctx: RknnContext = 0;
        // SAFETY: model buffer outlives the call; extend pointers are optional.
        let rc = unsafe {
            (api.init)(
                &mut ctx,
                model.as_mut_ptr().cast(),
                model.len() as u32,
                0,
                ptr::null_mut(),
            )
        };
        if rc != RKNN_SUCC {
            return Err(EdgeError::AiPanic(format!(
                "rknn_init failed ({rc}) for {} — is this a .rknn model converted for this SoC?",
                cfg.model_path
            )));
        }

        let mut version = RknnSdkVersion {
            api_version: [0; 256],
            drv_version: [0; 256],
        };
        // SAFETY: struct matches rknn_sdk_version; best-effort query.
        let vrc = unsafe {
            (api.query)(
                ctx,
                RKNN_QUERY_SDK_VERSION,
                (&mut version as *mut RknnSdkVersion).cast(),
                std::mem::size_of::<RknnSdkVersion>() as u32,
            )
        };

        let mut io_num = RknnInputOutputNum {
            n_input: 0,
            n_output: 0,
        };
        // SAFETY: struct matches rknn_input_output_num.
        let rc = unsafe {
            (api.query)(
                ctx,
                RKNN_QUERY_IN_OUT_NUM,
                (&mut io_num as *mut RknnInputOutputNum).cast(),
                std::mem::size_of::<RknnInputOutputNum>() as u32,
            )
        };
        if rc != RKNN_SUCC || io_num.n_input == 0 || io_num.n_output == 0 {
            unsafe { (api.destroy)(ctx) };
            return Err(EdgeError::AiPanic(format!(
                "rknn_query IN_OUT_NUM failed ({rc}); inputs={} outputs={}",
                io_num.n_input, io_num.n_output
            )));
        }

        let mut output_shapes = Vec::with_capacity(io_num.n_output as usize);
        for index in 0..io_num.n_output {
            let mut attr = RknnTensorAttr {
                index,
                ..RknnTensorAttr::default()
            };
            // SAFETY: struct matches rknn_tensor_attr (RKNPU2 ≥ 1.3 layout).
            let rc = unsafe {
                (api.query)(
                    ctx,
                    RKNN_QUERY_OUTPUT_ATTR,
                    (&mut attr as *mut RknnTensorAttr).cast(),
                    std::mem::size_of::<RknnTensorAttr>() as u32,
                )
            };
            if rc != RKNN_SUCC {
                unsafe { (api.destroy)(ctx) };
                return Err(EdgeError::AiPanic(format!(
                    "rknn_query OUTPUT_ATTR[{index}] failed ({rc}) — runtime/header ABI mismatch?"
                )));
            }
            let dims = attr.dims[..(attr.n_dims as usize).min(RKNN_MAX_DIMS)]
                .iter()
                .map(|&d| i64::from(d))
                .collect();
            output_shapes.push(dims);
        }

        let api_version = if vrc == RKNN_SUCC {
            cstr(&version.api_version)
        } else {
            String::new()
        };
        info!(
            model = %cfg.model_path,
            sha256 = %sha256,
            size_bytes = model.len(),
            inputs = io_num.n_input,
            outputs = io_num.n_output,
            output_shapes = ?output_shapes,
            api_version = %api_version,
            "RKNN NPU session ready"
        );
        if io_num.n_input > 1 {
            warn!(
                "model declares {} inputs; this backend feeds input 0 only",
                io_num.n_input
            );
        }

        Ok(Self {
            ctx,
            api,
            _lib: lib,
            n_outputs: io_num.n_output,
            output_shapes,
            nhwc_scratch: Vec::new(),
            input_width: cfg.input_width.max(1) as f32,
            input_height: cfg.input_height.max(1) as f32,
        })
    }

    /// Converts `input` (NCHW-planar, [0,1]-normalized — preprocess.rs's
    /// shared output, also consumed as-is by the ONNX backend) into
    /// `self.nhwc_scratch` (NHWC-interleaved, [0,255]-scaled). Resizes only
    /// when the input size changes (it won't, in practice —
    /// `ai.input_width`/`input_height` are fixed for the runtime's
    /// lifetime) rather than reallocating every call.
    ///
    /// The x255 is not optional: this model was converted with
    /// `rknn.config(mean_values=[[0,0,0]], std_values=[[255,255,255]])`
    /// (rknn_model_zoo/examples/yolov8/python/convert.py) — RKNPU2 applies
    /// `(px - mean) / std` internally as part of the "normalize" step
    /// referenced in the layout error this backend already works around,
    /// so it expects raw [0,255] pixel values and does its own /255. Feeding
    /// it preprocess.rs's already-normalized [0,1] output means every pixel
    /// gets divided by 255 a second time (128 -> 0.50 -> 0.002), which is
    /// indistinguishable from a black frame to the model — confirmed
    /// on-device 2026-07-19: inference ran error-free but produced zero
    /// detections for every class with a real subject in frame.
    fn nchw_to_nhwc(&mut self, input: &[f32], width: usize, height: usize, channels: usize) {
        if self.nhwc_scratch.len() != input.len() {
            self.nhwc_scratch.resize(input.len(), 0.0);
        }
        for c in 0..channels {
            for y in 0..height {
                for x in 0..width {
                    // Planar index: C*H*W + Y*W + X
                    let src_idx = c * (width * height) + y * width + x;
                    // Interleaved index: (Y*W + X)*C + C
                    let dst_idx = (y * width + x) * channels + c;
                    self.nhwc_scratch[dst_idx] = input[src_idx] * 255.0;
                }
            }
        }
    }

    /// This model's detection head (see `src/ai/parser.rs::YoloV8Parser`,
    /// the same [1, 4+nc, anchors] head layout) reports box coordinates
    /// (cx, cy, w, h — the first 4 of the 4+nc channels) normalized to
    /// [0,1] as a fraction of the model input size, not the absolute
    /// input-pixel-space (0..input_width/height) that a standard Ultralytics
    /// export produces and that the shared parser expects. Confirmed
    /// on-device 2026-07-19: every reported box came back as `w:1,h:1`
    /// regardless of the actual subject's size in frame — precisely what
    /// dividing an already-tiny (~0.3) normalized width by the ~0.33
    /// letterbox scale factor produces, while class-score channels (read via
    /// the identical tensor-orientation logic) were already producing sane,
    /// consistent labels — isolating the bug to these 4 channels specifically
    /// rather than a wider indexing/orientation problem. Scaling here (not
    /// in the shared parser) keeps the ONNX backend's expected convention
    /// untouched.
    fn denormalize_boxes(&self, data: &mut [f32], shape: &[i64]) {
        if shape.len() != 3 || shape[0] != 1 {
            return;
        }
        let (d1, d2) = (shape[1] as usize, shape[2] as usize);
        let (attrs, anchors, attrs_first) = if d1 <= d2 {
            (d1, d2, true)
        } else {
            (d2, d1, false)
        };
        if attrs < 5 {
            return;
        }
        for a in 0..anchors {
            for (channel, scale) in [
                (0, self.input_width),
                (1, self.input_height),
                (2, self.input_width),
                (3, self.input_height),
            ] {
                let idx = if attrs_first {
                    channel * anchors + a
                } else {
                    a * attrs + channel
                };
                data[idx] *= scale;
            }
        }
    }
}

impl InferenceRuntime for RknnRuntime {
    fn name(&self) -> &'static str {
        "rknn"
    }

    fn infer(&mut self, input: InputTensor) -> EdgeResult<Vec<OutputTensor>> {
        // preprocess.rs builds an NCHW-planar tensor (see InputTensor::shape:
        // [batch, channels, height, width]), but this RKNPU2 runtime's input
        // normalize step only accepts NHWC-interleaved source data — on
        // physical hardware `rknn_inputs_set` fails outright with "Meet
        // unsupported src layout for normalize: NCHW, only support NHWC src
        // layout!" (not silently wrong, an explicit rejected call). Convert
        // here rather than in preprocess.rs so the ONNX backend keeps
        // consuming the NCHW layout it expects.
        let [_batch, channels, height, width] = input.shape;
        self.nchw_to_nhwc(
            &input.data,
            width as usize,
            height as usize,
            channels as usize,
        );
        let mut rknn_input = RknnInput {
            index: 0,
            buf: self.nhwc_scratch.as_mut_ptr().cast(),
            size: (self.nhwc_scratch.len() * std::mem::size_of::<f32>()) as u32,
            pass_through: 0,
            ttype: RKNN_TENSOR_FLOAT32,
            fmt: RKNN_TENSOR_NHWC,
        };
        // SAFETY: buffer lives across inputs_set + run; runtime copies/quantizes.
        let rc = unsafe { (self.api.inputs_set)(self.ctx, 1, &mut rknn_input) };
        if rc != RKNN_SUCC {
            return Err(EdgeError::AiPanic(format!("rknn_inputs_set failed ({rc})")));
        }

        let rc = unsafe { (self.api.run)(self.ctx, ptr::null_mut()) };
        if rc != RKNN_SUCC {
            return Err(EdgeError::AiPanic(format!("rknn_run failed ({rc})")));
        }

        let mut outputs: Vec<RknnOutput> = (0..self.n_outputs)
            .map(|index| RknnOutput {
                want_float: 1, // runtime dequantizes for us
                is_prealloc: 0,
                index,
                buf: ptr::null_mut(),
                size: 0,
            })
            .collect();
        // SAFETY: outputs array sized n_outputs; released below.
        let rc = unsafe {
            (self.api.outputs_get)(
                self.ctx,
                self.n_outputs,
                outputs.as_mut_ptr(),
                ptr::null_mut(),
            )
        };
        if rc != RKNN_SUCC {
            return Err(EdgeError::AiPanic(format!(
                "rknn_outputs_get failed ({rc})"
            )));
        }

        let mut result = Vec::with_capacity(outputs.len());
        for (output, shape) in outputs.iter().zip(&self.output_shapes) {
            let elems = output.size as usize / std::mem::size_of::<f32>();
            // SAFETY: with want_float=1 buf holds `elems` f32s until release.
            let mut data =
                unsafe { std::slice::from_raw_parts(output.buf as *const f32, elems) }.to_vec();
            self.denormalize_boxes(&mut data, shape);
            result.push(OutputTensor {
                data,
                shape: shape.clone(),
            });
        }

        // SAFETY: same array passed to outputs_get.
        unsafe { (self.api.outputs_release)(self.ctx, self.n_outputs, outputs.as_mut_ptr()) };
        Ok(result)
    }
}

impl Drop for RknnRuntime {
    fn drop(&mut self) {
        // SAFETY: ctx came from rknn_init and is destroyed exactly once.
        unsafe { (self.api.destroy)(self.ctx) };
    }
}

fn load_runtime_library() -> EdgeResult<Library> {
    // RKNPU2 runtime first; legacy RK1808-era name as fallback.
    for name in ["librknnrt.so", "librknn_api.so"] {
        // SAFETY: loading a shared library; constructors run, as with any dlopen.
        if let Ok(lib) = unsafe { Library::new(name) } {
            info!("Loaded Rockchip NPU runtime: {name}");
            return Ok(lib);
        }
    }
    Err(EdgeError::AiPanic(
        "Rockchip NPU runtime not found (librknnrt.so). Install the RKNPU2 runtime \
         (Radxa OS: apt install rknpu2-rk356x or vendor equivalent), or set \
         ai.runtime = \"onnx\" for CPU inference."
            .into(),
    ))
}

/// Look up a symbol and detach it from the borrow of `lib` (we keep the
/// Library alive for the runtime's lifetime in RknnRuntime).
unsafe fn get<T>(lib: &Library, name: &[u8]) -> EdgeResult<libloading::os::unix::Symbol<T>> {
    let symbol: Symbol<T> = lib.get(name).map_err(|e| {
        EdgeError::AiPanic(format!(
            "librknnrt is missing symbol {}: {e}",
            String::from_utf8_lossy(name)
        ))
    })?;
    Ok(symbol.into_raw())
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
