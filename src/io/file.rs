//! File abstractions used with the [`IoUringReactor`](crate::reactor::IoUringReactor).
//! The [`DmaFile`] type wraps a standard `File` and provides asynchronous
//! read/write helpers returning futures.

use std::{fs::File, os::fd::AsRawFd, rc::Rc};

use crate::reactor::{allocator::FixedBuffer, IoUringReactor};




/// Wrapper around [`File`] that performs DMA capable I/O via `io_uring`.
pub struct DmaFile {
    pub file: File,
    pub reactor: Rc<IoUringReactor>,
}

impl DmaFile {
    /// Create a new `DmaFile` backed by `file` and associated reactor.
    pub fn new(file: File, reactor: Rc<IoUringReactor>) -> Self {
        DmaFile { file, reactor }
    }

    /// Read into the provided buffer at the given offset.
    pub async fn read(&self, mut buffer: Vec<u8>,  offset: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Read::new(
            io_uring::types::Fd(fd),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
        .offset(offset)
        .build();

        self.reactor.push_sqe(sqe).await
    }

    /// Write the buffer at the specified offset.
    pub async fn write(&self, buffer: Vec<u8>, offset: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Write::new(
            io_uring::types::Fd(fd),
            buffer.as_ptr(),
            buffer.len() as u32,
        )
        .offset(offset)
        .build();

        self.reactor.push_sqe(sqe).await
    }

    /// Perform a `ReadFixed` using a pre-registered buffer.
    pub async fn read_fixed(&self, buffer: FixedBuffer, offset: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::ReadFixed::new(
            io_uring::types::Fd(fd),
            buffer.as_ptr() as *mut u8,
            buffer.len() as u32,
            buffer.index() as u16,
        )
        .offset(offset)
        .build();

        self.reactor.push_sqe(sqe).await
    }

    /// Perform a `WriteFixed` using a pre-registered buffer.
    pub async fn write_fixed(&self, buffer: FixedBuffer, offset: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = { 
            io_uring::opcode::WriteFixed::new(
                io_uring::types::Fd(fd),
                buffer.as_ptr(),
                buffer.len() as u32,
                buffer.index() as u16,
            )
            .offset(offset)
            .build()
        };

        self.reactor.push_sqe(sqe).await
    }

    /// Acquire a fixed buffer from the reactor's allocator.
    pub async fn acquire_buffer(&self) -> FixedBuffer {
        self.reactor.acquire_buffer().await
    }
}