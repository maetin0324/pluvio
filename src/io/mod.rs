use crate::reactor::Reactor;
use allocator::{FixedBufferAllocator, WriteFixedBuffer};
use io_uring::types;
use std::cell::RefCell;
use std::future::Future;
// use std::io::Result;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

pub mod allocator;

pub struct HandleState<T> {
    pub waker: RefCell<Option<std::task::Waker>>,
    pub result: RefCell<Option<Result<T, String>>>,
}
impl<T> HandleState<T> {
    pub fn new() -> Self {
        HandleState {
            waker: RefCell::new(None),
            result: RefCell::new(None),
        }
    }
}
pub struct ReadFileFuture {
    handle_state: Rc<RefCell<HandleState<usize>>>,
    fd: i32,
    buffer: Vec<u8>,
    offset: u64,
    reactor: Rc<Reactor>,
}

impl ReadFileFuture {
    pub fn new(fd: i32, buffer: Vec<u8>, offset: u64, reactor: Rc<Reactor>) -> Self {
        ReadFileFuture {
            handle_state: Rc::new(RefCell::new(HandleState::new())),
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
        let mut handle = this.handle_state.borrow_mut();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = handle.result.borrow_mut().take() {
            tracing::trace!("ReadFileFuture completed, read {} bytes", this.buffer.len());
            return Poll::Ready(result);
        }

        // I/O 操作をまだ登録していない場合、登録する
        if handle.waker.borrow_mut().is_none() {
            // Reactor に HandleState を登録し、user_data を取得
            let user_data = this.reactor.register_io(this.handle_state.clone());

            // SQE の取得
            let sqe = {
                io_uring::opcode::Read::new(
                    types::Fd(this.fd),
                    this.buffer.as_mut_ptr(),
                    this.buffer.len() as u32,
                )
                .offset(this.offset)
                .build()
                // let mut ring = this.reactor.ring.borrow_mut();
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
        handle.waker = RefCell::new(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub struct WriteFileFuture {
    handle_state: Rc<RefCell<HandleState<usize>>>,
    fd: i32,
    buffer: Vec<u8>,
    offset: u64,
    reactor: Rc<Reactor>,
}

impl WriteFileFuture {
    pub fn new(fd: i32, buffer: Vec<u8>, offset: u64, reactor: Rc<Reactor>) -> Self {
        WriteFileFuture {
            handle_state: Rc::new(RefCell::new(HandleState::new())),
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
        let mut handle = this.handle_state.borrow_mut();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = handle.result.borrow_mut().take() {
            tracing::trace!(
                "WriteFileFuture completed, wrote {} bytes",
                this.buffer.len()
            );
            return Poll::Ready(result);
        }

        // I/O 操作をまだ登録していない場合、登録する
        if handle.waker.borrow_mut().is_none() {
            // Reactor に HandleState を登録し、user_data を取得
            let user_data = this.reactor.register_io(this.handle_state.clone());

            // SQE の準備
            let sqe = {
                io_uring::opcode::Write::new(
                    types::Fd(this.fd),
                    this.buffer.as_ptr() as *const _,
                    this.buffer.len() as u32,
                )
                .offset(this.offset)
                .build()
                // let mut ring = this.reactor.ring.borrow_mut();
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
        handle.waker = RefCell::new(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub struct WriteFixedFuture {
    handle_state: Rc<RefCell<HandleState<usize>>>,
    sqe: io_uring::squeue::Entry,
    reactor: Rc<Reactor>,
}

impl WriteFixedFuture {
    pub fn new(sqe: io_uring::squeue::Entry, reactor: Rc<Reactor>) -> Self {
        WriteFixedFuture {
            handle_state: Rc::new(RefCell::new(HandleState::new())),
            sqe,
            reactor,
        }
    }
}


impl Future for WriteFixedFuture {
    type Output = Result<usize, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut handle = this.handle_state.borrow_mut();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = handle.result.borrow_mut().take() {
            return Poll::Ready(result);
        }

        // I/O 操作をまだ登録していない場合、登録する
        if handle.waker.borrow_mut().is_none() {
            // Reactor に HandleState を登録し、user_data を取得
            let user_data = this.reactor.register_io(this.handle_state.clone());

            // SQE の準備
            let sqe = std::mem::replace(&mut this.sqe, io_uring::opcode::Nop::new().build());

            // Write 操作を準備
            let sqe = sqe.user_data(user_data);

            // I/O 操作を送信
            this.reactor.submit_io(sqe, user_data);
        }

        // Waker を保存してタスクを再開可能にする
        handle.waker = RefCell::new(Some(cx.waker().clone()));
        Poll::Pending
    }
}

pub fn prepare_buffer(mut allocator: Rc<FixedBufferAllocator>) -> Option<WriteFixedBuffer> {
    let buffer = allocator.acquire();
    buffer
}

pub fn write_fixed(fd: i32, offset: u64, buffer: WriteFixedBuffer, reactor: Rc<Reactor>) -> WriteFixedFuture {
    let sqe = buffer.prepare_sqe(fd, offset);
    WriteFixedFuture::new(sqe, reactor)
}