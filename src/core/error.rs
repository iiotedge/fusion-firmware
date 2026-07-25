// src/core/error.rs
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EdgeError {
    // Constructed by the Linux-only V4L2/ISP backends; on non-Linux dev hosts
    // the dead-code analyzer can't see that.
    #[allow(dead_code)]
    #[error("Hardware Failure: {0}")]
    HardwareFault(String),

    #[error("Frame Dropped: Queue Capacity Exceeded")]
    QueueFull,

    #[error("AI Engine Panic: {0}")]
    AiPanic(String),

    #[error("Streaming Failure: {0}")]
    StreamFault(String),

    // Constructed by core::memory, which goes live with HAL v2 (Phase 2).
    #[allow(dead_code)]
    #[error("V4L2 DMA Buffer mapping failed")]
    MemoryMapFailed,
}

pub type EdgeResult<T> = Result<T, EdgeError>;
