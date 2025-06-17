use std::{fs::File, os::fd::AsRawFd};

use crate::reactor::{allocator::FixedBuffer, IoUringReactor};




pub struct DmaFile {
    pub file: File,
}

impl DmaFile {
    pub fn new(file: File) -> Self {
        DmaFile { file }
    }

    pub async fn read(&self, mut buffer: Vec<u8>,  offset: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Read::new(
            io_uring::types::Fd(fd),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
        .offset(offset)
        .build();

        let reactor = IoUringReactor::get_or_init();

        reactor.push_sqe(sqe).await
    }

    pub async fn write(&self, buffer: Vec<u8>, offset: u64) -> std::io::Result<i32> {
        let fd = self.file.as_raw_fd();
        let sqe = io_uring::opcode::Write::new(
            io_uring::types::Fd(fd),
            buffer.as_ptr(),
            buffer.len() as u32,
        )
        .offset(offset)
        .build();

        let reactor = IoUringReactor::get_or_init();

        reactor.push_sqe(sqe).await
    }

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

        let reactor = IoUringReactor::get_or_init();

        reactor.push_sqe(sqe).await
    }

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

        let reactor = IoUringReactor::get_or_init();

        reactor.push_sqe(sqe).await
    }

    pub async fn acquire_buffer(&self) -> FixedBuffer {
        let reactor = IoUringReactor::get_or_init();
        reactor.acquire_buffer().await
    }
}