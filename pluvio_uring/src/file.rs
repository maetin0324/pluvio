//! File abstractions used with the [`IoUringReactor`](crate::reactor::IoUringReactor).
//! The [`DmaFile`] type wraps a standard `File` and provides asynchronous
//! read/write helpers returning futures.

use std::{fs::File, os::fd::{AsRawFd, FromRawFd}, rc::Rc};

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
    /// `len` is the number of bytes to request from the kernel. It must be
    /// `<= buffer.len()`; pass the buffer's full capacity to read as much as
    /// will fit. When the file is opened with `O_DIRECT`, `len` must also be a
    /// multiple of the device block size — non-aligned `len` callers should
    /// open the file as buffered I/O instead.
    ///
    /// Returns a tuple of (bytes_read, buffer) so the caller can access
    /// the data read into the buffer.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self, buffer))]
    pub async fn read_fixed(
        &self,
        buffer: FixedBuffer,
        offset: u64,
        len: u32,
    ) -> std::io::Result<(i32, FixedBuffer)> {
        debug_assert!(
            len as usize <= buffer.len(),
            "read_fixed len ({}) must be <= buffer.len() ({})",
            len,
            buffer.len(),
        );
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::ReadFixed::new(
            io_uring::types::Fd(fd),
            buffer.as_ptr() as *mut u8,
            len,
            buffer.index() as u16,
        )
        .offset(offset)
        .build();

        let result = self.reactor.push_sqe(sqe).await;
        result.map(|bytes_read| (bytes_read, buffer))
    }

    /// Perform a `WriteFixed` using a pre-registered buffer.
    ///
    /// `len` is the number of bytes to write. It must be `<= buffer.len()`.
    /// When the file is opened with `O_DIRECT`, `len` must also be a multiple
    /// of the device block size — non-aligned `len` callers should open the
    /// file as buffered I/O instead.
    ///
    /// Returns a tuple of (bytes_written, buffer) so the caller can
    /// reuse the buffer if needed.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self, buffer))]
    pub async fn write_fixed(
        &self,
        buffer: FixedBuffer,
        offset: u64,
        len: u32,
    ) -> std::io::Result<(i32, FixedBuffer)> {
        debug_assert!(
            len as usize <= buffer.len(),
            "write_fixed len ({}) must be <= buffer.len() ({})",
            len,
            buffer.len(),
        );
        let fd = self.file.as_raw_fd();
        let sqe = {
            io_uring::opcode::WriteFixed::new(
                io_uring::types::Fd(fd),
                buffer.as_ptr(),
                len,
                buffer.index() as u16,
            )
            .offset(offset)
            .build()
        };

        let result = self.reactor.push_sqe(sqe).await;
        result.map(|bytes_written| (bytes_written, buffer))
    }

    /// `write_fixed` variant that does not take ownership of a `FixedBuffer`.
    /// The caller supplies the registered buffer's (ptr, len, io_uring fixed
    /// buffer index) directly. Useful when the underlying buffer is held by
    /// another subsystem (e.g. an `Arc<FixedBuffer>` reachable from RDMA
    /// transport state) and ownership transfer is impractical.
    ///
    /// # Safety
    /// `ptr` must point to a buffer that was registered with this
    /// `IoUringReactor` via `register_buffers`, and `buf_index` must be the
    /// 0-based slot the kernel knows it as. `len` must be ≤ the registered
    /// buffer's size, and the memory must stay valid until the SQE
    /// completes.
    // async_backtrace removed from hot path (benchfs ior-hard 320k RPC/s).
    pub async fn write_fixed_raw(
        &self,
        offset: u64,
        ptr: *const u8,
        len: u32,
        buf_index: u16,
    ) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::WriteFixed::new(
            io_uring::types::Fd(fd),
            ptr,
            len,
            buf_index,
        )
        .offset(offset)
        .build();
        self.reactor.push_sqe(sqe).await
    }

    /// `read_fixed` variant that does not take ownership of a `FixedBuffer`.
    /// See [`write_fixed_raw`] for safety requirements.
    // async_backtrace removed from hot path (benchfs ior-hard 320k RPC/s).
    pub async fn read_fixed_raw(
        &self,
        offset: u64,
        ptr: *mut u8,
        len: u32,
        buf_index: u16,
    ) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::ReadFixed::new(
            io_uring::types::Fd(fd),
            ptr,
            len,
            buf_index,
        )
        .offset(offset)
        .build();
        self.reactor.push_sqe(sqe).await
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

    /// Open a file asynchronously using io_uring with the thread-local reactor.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(path))]
    pub async fn open(path: &str, flags: i32, mode: u32) -> std::io::Result<DmaFile> {
        let reactor = IoUringReactor::get_or_init();
        Self::open_with_reactor(path, flags, mode, reactor).await
    }

    /// Open a file asynchronously using io_uring with an explicit reactor.
    ///
    /// This variant allows specifying a reactor instead of using the thread-local one,
    /// which is important when the caller needs to ensure the DmaFile uses the same
    /// reactor that registered fixed buffers.
    ///
    /// # Arguments
    /// * `path` - File path as a string slice
    /// * `flags` - Linux open flags (e.g., O_RDONLY, O_WRONLY | O_CREAT | O_DIRECT)
    /// * `mode` - File permissions for creation (ignored if O_CREAT not set)
    /// * `reactor` - The io_uring reactor to use
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(path, reactor))]
    pub async fn open_with_reactor(
        path: &str,
        flags: i32,
        mode: u32,
        reactor: Rc<IoUringReactor>,
    ) -> std::io::Result<DmaFile> {
        let path_cstr = std::ffi::CString::new(path).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains null byte")
        })?;

        let sqe = io_uring::opcode::OpenAt::new(
            io_uring::types::Fd(libc::AT_FDCWD),
            path_cstr.as_ptr(),
        )
        .flags(flags)
        .mode(mode)
        .build();

        let fd = reactor.push_sqe(sqe).await?;
        let file = unsafe { File::from_raw_fd(fd) };
        Ok(DmaFile { file, reactor })
    }

    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn close(&self) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Close::new(io_uring::types::Fd(fd)).build();

        self.reactor.push_sqe(sqe).await
    }

    /// Stat a path asynchronously via io_uring (`IORING_OP_STATX`).
    ///
    /// `flags` and `mask` follow the libc `statx(2)` semantics:
    ///
    /// - `flags`: `AT_STATX_SYNC_AS_STAT` (0) for default behavior,
    ///   `AT_SYMLINK_NOFOLLOW` for lstat-style behavior, etc.
    /// - `mask`: `STATX_BASIC_STATS` (= 0x07ff) for the common subset
    ///   (type/mode/nlink/uid/gid/atime/mtime/ctime/ino/size/blocks).
    ///
    /// On success, returns the kernel-filled `libc::statx` struct.
    /// Uses `AT_FDCWD` so the path is resolved relative to the process'
    /// CWD (BenchFS keeps a single CWD because it runs single-threaded).
    ///
    /// Why bother going async here? `find` / mdtest-stat phases issue
    /// hundreds of thousands of stats; doing each via blocking `statx(2)`
    /// would burn the reactor thread on syscalls. `IORING_OP_STATX` lets
    /// us batch many stats per `io_uring_enter`.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(path))]
    pub async fn statx_path(
        path: &str,
        flags: i32,
        mask: u32,
    ) -> std::io::Result<libc::statx> {
        let reactor = IoUringReactor::get_or_init();
        Self::statx_path_with_reactor(path, flags, mask, reactor).await
    }

    /// Same as [`Self::statx_path`] but with an explicit reactor.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(path, reactor))]
    pub async fn statx_path_with_reactor(
        path: &str,
        flags: i32,
        mask: u32,
        reactor: Rc<IoUringReactor>,
    ) -> std::io::Result<libc::statx> {
        let path_cstr = std::ffi::CString::new(path).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains null byte")
        })?;

        // The kernel writes the result here. Box it so the pointer is
        // stable while the future is suspended.
        let mut statx_buf: Box<libc::statx> = unsafe { Box::new(std::mem::zeroed()) };

        let sqe = io_uring::opcode::Statx::new(
            io_uring::types::Fd(libc::AT_FDCWD),
            path_cstr.as_ptr(),
            // libc::statx and io_uring::types::statx have identical
            // memory layout per the io-uring crate docs — the opaque
            // `statx` is only opaque to discourage callers from poking
            // at it directly.
            (&mut *statx_buf as *mut libc::statx).cast::<io_uring::types::statx>(),
        )
        .flags(flags)
        .mask(mask)
        .build();

        let _ = reactor.push_sqe(sqe).await?;
        // path_cstr and statx_buf were live through the await.
        Ok(*statx_buf)
    }

    /// Acquire a fixed buffer from the reactor's allocator.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn acquire_buffer(&self) -> FixedBuffer {
        self.reactor.acquire_buffer().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::IoUringReactorBuilder;
    use pluvio_runtime::executor::{set_runtime, Runtime};

    #[test]
    fn statx_returns_file_size() {
        // Use a unique temp file so this test is hermetic.
        let path = format!(
            "/tmp/pluvio_uring_statx_test_{}.bin",
            std::process::id()
        );
        std::fs::write(&path, b"hello, statx").expect("write tmp file");
        let metadata = std::fs::metadata(&path).expect("metadata");
        let expected_size = metadata.len();

        let runtime = Runtime::new(64);
        set_runtime(runtime.clone());
        let reactor = IoUringReactorBuilder::default().build();
        IoUringReactor::init(reactor.clone()).ok();
        runtime.register_reactor("iouring", reactor.clone());

        let path_for_async = path.clone();
        runtime.block_on_with_name_and_runtime("statx_test", async move {
            let st = DmaFile::statx_path(
                &path_for_async,
                0,
                libc::STATX_BASIC_STATS,
            )
            .await
            .expect("statx");
            assert_eq!(st.stx_size, expected_size);
            // Regular file — top bits of stx_mode should be S_IFREG.
            let ftype = st.stx_mode as u32 & libc::S_IFMT;
            assert_eq!(ftype, libc::S_IFREG);
        });

        let _ = std::fs::remove_file(&path);
    }
}
