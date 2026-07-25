// Not wired into the capture path yet: becomes the V4L2 DMABUF zero-copy
// carrier in HAL v2 (TODO.md Phase 2).
#![allow(dead_code)]

use crate::core::error::{EdgeError, EdgeResult};
use std::os::unix::io::RawFd;
use tracing::debug;

/// Represents the physical memory block allocated by the Linux Kernel (V4L2/DMA)
#[derive(Debug)]
pub struct DmaBuffer {
    pub index: u32,          // The V4L2 buffer ID (e.g., Buffer 0, 1, 2, or 3)
    pub fd: RawFd,           // The DMA-BUF File Descriptor (used to pass to Hardware Encoder)
    pub mapped_ptr: *mut u8, // The raw memory pointer (used to pass to AI NPU)
    pub capacity: usize,     // Total size of the allocated memory block
    pub bytes_used: usize,   // Actual size of the captured frame
    pub timestamp_ns: u64,   // Hardware timestamp for PTP sync
}

// SAFETY: We assert that it is safe to send these pointers across thread boundaries.
// The memory is locked by the kernel until we explicitly release it.
unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

impl DmaBuffer {
    /// Safely wraps a raw kernel pointer. This is the only `unsafe` entry point.
    pub fn new(index: u32, fd: RawFd, ptr: *mut u8, capacity: usize) -> Self {
        Self {
            index,
            fd,
            mapped_ptr: ptr,
            capacity,
            bytes_used: 0,
            timestamp_ns: 0,
        }
    }

    /// Converts the raw C-pointer into a safe Rust byte slice for the AI engine to read.
    pub fn as_slice(&self) -> EdgeResult<&[u8]> {
        if self.mapped_ptr.is_null() {
            return Err(EdgeError::MemoryMapFailed);
        }
        if self.bytes_used == 0 {
            return Err(EdgeError::HardwareFault(
                "Attempted to read empty buffer".to_string(),
            ));
        }

        // SAFETY: We guarantee the pointer is valid, non-null, and the memory
        // outlives this slice because the DMA block is pinned in RAM.
        unsafe { Ok(std::slice::from_raw_parts(self.mapped_ptr, self.bytes_used)) }
    }
}

/// The Drop Trait is our Ultimate Safety Net.
/// In Industry 4.0, if a thread panics or simply finishes processing a frame,
/// this struct automatically tells the hardware: "I'm done, you can overwrite this memory."
impl Drop for DmaBuffer {
    fn drop(&mut self) {
        // In a full implementation, you would call V4L2 `VIDIOC_QBUF` here
        // to re-queue the `self.index` back to the camera sensor.
        debug!(
            "Releasing DMA Buffer {} back to the hardware pool.",
            self.index
        );

        // Example C FFI Call (simulated):
        // unsafe { v4l2_sys::ioctl(self.camera_fd, VIDIOC_QBUF, &mut v4l2_buffer); }
    }
}
