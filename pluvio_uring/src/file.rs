//! File abstractions used with the [`IoUringReactor`](crate::reactor::IoUringReactor).
//! The [`DmaFile`] type wraps a standard `File` and provides asynchronous
//! read/write helpers returning futures.

use std::{fs::File, os::fd::AsRawFd, rc::Rc};

use crate::{allocator::FixedBuffer, reactor::IoUringReactor};

/// Wrapper around [`File`] that performs DMA capable I/O via `io_uring`.
pub struct DmaFile {
    file: File,
    reactor: Rc<IoUringReactor>,
}

impl DmaFile {
    /// Create a new `DmaFile` backed by `file` using the thread-local reactor.
    ///
    /// # Panics
    /// Panics if no reactor has been initialized via `IoUringReactorBuilder::build()`.
    /// This ensures that fixed buffers registered with the reactor are valid for
    /// operations like `ReadFixed` and `WriteFixed`.
    #[tracing::instrument(level = "trace", skip(file))]
    pub fn new(file: File) -> Self {
        let reactor = IoUringReactor::get_or_init();
        DmaFile { file, reactor }
    }

    /// Create a new `DmaFile` backed by `file` and an explicitly provided reactor.
    ///
    /// Use this method when you need to ensure the `DmaFile` uses a specific reactor,
    /// particularly important when using fixed buffers that are registered with
    /// a particular io_uring instance.
    #[tracing::instrument(level = "trace", skip(file, reactor))]
    pub fn with_reactor(file: File, reactor: Rc<IoUringReactor>) -> Self {
        DmaFile { file, reactor }
    }

    /// Read into the provided buffer at the given offset.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self, buffer))]
    pub async fn read(&self, mut buffer: Vec<u8>, offset: u64) -> std::io::Result<i32> {
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
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self, buffer))]
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
    ///
    /// Returns a tuple of (bytes_read, buffer) so the caller can access
    /// the data read into the buffer.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self, buffer))]
    pub async fn read_fixed(
        &self,
        buffer: FixedBuffer,
        offset: u64,
    ) -> std::io::Result<(i32, FixedBuffer)> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::ReadFixed::new(
            io_uring::types::Fd(fd),
            buffer.as_ptr() as *mut u8,
            buffer.len() as u32,
            buffer.index() as u16,
        )
        .offset(offset)
        .build();

        let result = self.reactor.push_sqe(sqe).await;
        result.map(|bytes_read| (bytes_read, buffer))
    }

    /// Perform a `WriteFixed` using a pre-registered buffer.
    ///
    /// Returns a tuple of (bytes_written, buffer) so the caller can
    /// reuse the buffer if needed.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self, buffer))]
    pub async fn write_fixed(
        &self,
        buffer: FixedBuffer,
        offset: u64,
    ) -> std::io::Result<(i32, FixedBuffer)> {
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

        let result = self.reactor.push_sqe(sqe).await;
        result.map(|bytes_written| (bytes_written, buffer))
    }

    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn fallocate(&self, size: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Fallocate::new(io_uring::types::Fd(fd), size).build();

        self.reactor.push_sqe(sqe).await
    }

    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn fsync(&self) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Fsync::new(io_uring::types::Fd(fd)).build();

        self.reactor.push_sqe(sqe).await
    }

    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn close(&self) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Close::new(io_uring::types::Fd(fd)).build();

        self.reactor.push_sqe(sqe).await
    }

    /// Acquire a fixed buffer from the reactor's allocator.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn acquire_buffer(&self) -> FixedBuffer {
        self.reactor.acquire_buffer().await
    }
}
