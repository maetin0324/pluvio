//! Asynchronous I/O helpers used by the runtime.
//!
//! This module provides future types for reading and writing files using
//! `io_uring` as well as utilities for acquiring registered buffers.
use crate::reactor::allocator::{FixedBuffer, FixedBufferAllocator};
use std::rc::Rc;

pub mod file;

pub async fn prepare_buffer(allocator: Rc<FixedBufferAllocator>) -> FixedBuffer {
    allocator.acquire().await
}
