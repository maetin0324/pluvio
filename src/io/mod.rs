use crate::reactor::Reactor;
use crate::task::SharedState;
use allocator::{FixedBufferAllocator, WriteFixedBuffer};
use io_uring::opcode::Write;
use io_uring::types;
use std::cell::RefCell;
use std::future::Future;
// use std::io::Result;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::vec;

pub mod allocator;

pub struct ReadFileFuture {
    shared_state: Arc<Mutex<SharedState<usize>>>,
    fd: i32,
    buffer: Vec<u8>,
    offset: u64,
    reactor: Arc<Reactor>,
}

impl ReadFileFuture {
    pub fn new(fd: i32, buffer: Vec<u8>, offset: u64, reactor: Arc<Reactor>) -> Self {
        ReadFileFuture {
            shared_state: Arc::new(Mutex::new(SharedState::new())),
            fd,
            buffer,
            offset,
            reactor,
        }
    }
}

impl Future for ReadFileFuture {
    type Output = Result<usize, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut shared = this.shared_state.lock().unwrap();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = shared.result.lock().unwrap().take() {
            tracing::trace!("ReadFileFuture completed, read {} bytes", this.buffer.len());
            return Poll::Ready(result);
        }

        // I/O 操作をまだ登録していない場合、登録する
        if shared.waker.lock().unwrap().is_none() {
            // Reactor に SharedState を登録し、user_data を取得
            let user_data = this.reactor.register_io(this.shared_state.clone());

            // SQE の取得
            let sqe = {
                io_uring::opcode::Read::new(
                    types::Fd(this.fd),
                    this.buffer.as_mut_ptr(),
                    this.buffer.len() as u32,
                )
                .offset(this.offset)
                .build()
                // let mut ring = this.reactor.ring.lock().unwrap();
                // match ring.submission().get_sqe() {
                //     Some(sqe) => sqe,
                //     None => {
                //         // SQE が利用できない場合は Pending を返す
                //         // 次回ポーリング時に再試行
                //         return Poll::Pending;
                //     }
                // }
            };

            // Read 操作を準備
            let sqe = sqe.user_data(user_data);

            // I/O 操作を送信
            this.reactor.submit_io(sqe, user_data);
        }

        // Waker を保存してタスクを再開可能にする
        shared.waker = Mutex::new(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub struct WriteFileFuture {
    shared_state: Arc<Mutex<SharedState<usize>>>,
    fd: i32,
    buffer: Vec<u8>,
    offset: u64,
    reactor: Arc<Reactor>,
}

impl WriteFileFuture {
    pub fn new(fd: i32, buffer: Vec<u8>, offset: u64, reactor: Arc<Reactor>) -> Self {
        WriteFileFuture {
            shared_state: Arc::new(Mutex::new(SharedState::new())),
            fd,
            buffer,
            offset,
            reactor,
        }
    }
}

impl Future for WriteFileFuture {
    type Output = Result<usize, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut shared = this.shared_state.lock().unwrap();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = shared.result.lock().unwrap().take() {
            tracing::trace!(
                "WriteFileFuture completed, wrote {} bytes",
                this.buffer.len()
            );
            return Poll::Ready(result);
        }

        // I/O 操作をまだ登録していない場合、登録する
        if shared.waker.lock().unwrap().is_none() {
            // Reactor に SharedState を登録し、user_data を取得
            let user_data = this.reactor.register_io(this.shared_state.clone());

            // SQE の準備
            let sqe = {
                io_uring::opcode::Write::new(
                    types::Fd(this.fd),
                    this.buffer.as_ptr() as *const _,
                    this.buffer.len() as u32,
                )
                .offset(this.offset)
                .build()
                // let mut ring = this.reactor.ring.lock().unwrap();
                // match ring.submission().next() {
                //     Some(sqe) => sqe,
                //     None => {
                //         // SQE が利用できない場合は Pending を返す
                //         return Poll::Pending;
                //     }
                // }
            };

            // Write 操作を準備
            let sqe = sqe.user_data(user_data);

            // I/O 操作を送信
            this.reactor.submit_io(sqe, user_data);
        }

        // Waker を保存してタスクを再開可能にする
        shared.waker = Mutex::new(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub struct WriteFixedFuture {
    shared_state: Arc<Mutex<SharedState<usize>>>,
    sqe: io_uring::squeue::Entry,
    reactor: Arc<Reactor>,
}

impl WriteFixedFuture {
    pub fn new(sqe: io_uring::squeue::Entry, reactor: Arc<Reactor>) -> Self {
        WriteFixedFuture {
            shared_state: Arc::new(Mutex::new(SharedState::new())),
            sqe,
            reactor,
        }
    }
}


impl Future for WriteFixedFuture {
    type Output = Result<usize, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut shared = this.shared_state.lock().unwrap();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = shared.result.lock().unwrap().take() {
            return Poll::Ready(result);
        }

        // I/O 操作をまだ登録していない場合、登録する
        if shared.waker.lock().unwrap().is_none() {
            // Reactor に SharedState を登録し、user_data を取得
            let user_data = this.reactor.register_io(this.shared_state.clone());

            // SQE の準備
            let sqe = std::mem::replace(&mut this.sqe, io_uring::opcode::Nop::new().build());

            // Write 操作を準備
            let sqe = sqe.user_data(user_data);

            // I/O 操作を送信
            this.reactor.submit_io(sqe, user_data);
        }

        // Waker を保存してタスクを再開可能にする
        shared.waker = Mutex::new(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub fn prepare_buffer(mut allocator: Rc<FixedBufferAllocator>) -> Option<WriteFixedBuffer> {
    let buffer = allocator.acquire();
    buffer
}

pub fn write_fixed(fd: i32, offset: u64, buffer: WriteFixedBuffer, reactor: Arc<Reactor>) -> WriteFixedFuture {
    let sqe = buffer.prepare_sqe(fd, offset);
    WriteFixedFuture::new(sqe, reactor)
}