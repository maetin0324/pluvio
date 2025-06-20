use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    io::Error,
    pin::Pin,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use io_uring::IoUring;

use crate::reactor::{allocator::{FixedBuffer, FixedBufferAllocator}, builder::IoUringReactorBuilder};

pub mod allocator;
pub mod builder;

// thread_local! {
//     static REACTOR: std::cell::OnceCell<IoUringReactor> = std::cell::OnceCell::new();
// }

pub enum ReactorStatus {
    Running,
    Stopped,
}

pub trait Reactor {
    fn poll(&self);
    fn status(&self) -> ReactorStatus;
}

impl<R: Reactor> Reactor for Rc<R> {
    fn poll(&self) {
        self.as_ref().poll();
    }

    fn status(&self) -> ReactorStatus {
        self.as_ref().status()
    }
}

pub struct IoUringReactor {
    pub ring: Rc<RefCell<IoUring>>,
    pub completions: Rc<RefCell<HashMap<u64, Rc<RefCell<HandleState<i32>>>>>>,
    pub user_data_counter: AtomicU64,
    pub allocator: Rc<FixedBufferAllocator>,
    last_submit_time: RefCell<std::time::Instant>,
    io_uring_params: IoUringParams,
    completed_count: Cell<u64>,
}

struct IoUringParams {
    submit_depth: u32,
    wait_submit_timeout: Duration,
    wait_complete_timeout: Duration,
}

pub struct HandleState<T> {
    pub waker: RefCell<Option<std::task::Waker>>,
    pub result: RefCell<Option<std::io::Result<T>>>,
}
impl<T> HandleState<T> {
    pub fn new() -> Self {
        HandleState {
            waker: RefCell::new(None),
            result: RefCell::new(None),
        }
    }
}

pub struct WaitHandle {
    pub handle_state: Rc<RefCell<HandleState<i32>>>,
}

impl WaitHandle {
    pub fn new(handle_state: Rc<RefCell<HandleState<i32>>>) -> Self {
        WaitHandle { handle_state }
    }
}

impl Future for WaitHandle {
    type Output = std::io::Result<i32>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let handle = this.handle_state.borrow();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = handle.result.borrow_mut().take() {
            tracing::trace!("WaitHandle completed with result: {:?}", result);
            return Poll::Ready(result);
        }

        // Waker を登録
        handle.waker.replace(Some(cx.waker().clone()));
        Poll::Pending
    }
}

impl IoUringReactor {
    pub fn new() -> Rc<Self> {
        IoUringReactorBuilder::default().build()
    }

    // pub fn get_or_init(
    //     queue_size: u32,
    //     submit_depth: u32,
    //     wait_timeout: Duration,
    // ) -> &IoUringReactor {
    //     REACTOR.with(|cell| {
    //         cell.get_or_init(move || IoUringReactor::new(queue_size, submit_depth, wait_timeout))
    //     })
    // }

    pub fn builder() -> IoUringReactorBuilder {
        IoUringReactorBuilder::new()
    }

    pub fn register_io(&self, handle_state: Rc<RefCell<HandleState<i32>>>) -> u64 {
        let user_data = self.user_data_counter.fetch_add(1, Ordering::Relaxed);
        let mut completions = self.completions.borrow_mut();
        completions.insert(user_data, handle_state);
        user_data
    }

    pub fn submit_io(&self, sqe: io_uring::squeue::Entry, user_data: u64) {
        tracing::trace!("Reactor::submit_io user_data: {}", user_data);
        let mut ring = self.ring.borrow_mut();
        unsafe {
            let sqe = sqe.user_data(user_data);
            ring.submission()
                .push(&sqe)
                .expect("Submission queue is full");
        }
    }

    pub fn push_sqe(&self, sqe: io_uring::squeue::Entry) -> WaitHandle {
        let user_data = self.user_data_counter.fetch_add(1, Ordering::Relaxed);
        let handle_state = Rc::new(RefCell::new(HandleState::new()));
        let mut completions = self.completions.borrow_mut();
        completions.insert(user_data, handle_state.clone());
        let sqe = sqe.user_data(user_data);
        let mut ring = self.ring.borrow_mut();
        unsafe {
            ring.submission()
                .push(&sqe)
                .expect("Submission queue is full");
        }

        WaitHandle::new(handle_state)
    }

    pub fn poll_submit_and_completions(&self) {
        let mut ring = self.ring.borrow_mut();

        // SQE の送信
        ring.submit().expect("Failed to submit SQE");
        // ring.submit_and_wait(submission_len / 2).expect("Failed to submit SQE");
        // ring.submit_and_wait(submission_len).expect("Failed to submit SQE");

        let mut last = self.last_submit_time.borrow_mut();
        *last = Instant::now();

        // CQの処理

        let cq = ring.completion();

        for cqe in cq {
            let user_data = cqe.user_data();

            // マッピングから SharedState を取得
            let handle_state_opt = {
                let mut completions = self.completions.borrow_mut();
                completions.remove(&user_data)
            };

            if let Some(handle_state) = handle_state_opt {
                self.completed_count.set(self.completed_count.get() + 1);
                let mut handle = handle_state.borrow_mut();
                if cqe.result() >= 0 {
                    handle.result = RefCell::new(Some(Ok(cqe.result())));
                } else {
                    // エラーハンドリング
                    handle.result =
                        RefCell::new(Some(Err(Error::from_raw_os_error(-cqe.result()))));
                }

                // Waker を呼び出してタスクを再開
                let waker_opt = handle.waker.borrow_mut().take();
                if let Some(waker) = waker_opt {
                    waker.wake();
                } else {
                    tracing::warn!("Waker is None for user_data: {}", user_data);
                    unreachable!("Waker is None for user_data: {}", user_data);
                }
            } else {
                tracing::warn!("Received completion for unknown user_data: {}", user_data);
                unreachable!("Received completion for unknown user_data");
            }
        }

        //
    }

    pub async fn acquire_buffer(&self) -> FixedBuffer {
        self.allocator.acquire().await
    }

    pub fn is_empty(&self) -> bool {
        let completions = self.completions.borrow_mut();
        completions.is_empty()
    }

    pub fn register_file(&self, fd: i32) {
        let ring = self.ring.borrow_mut();
        ring.submitter()
            .register_files(&[fd])
            .expect("Failed to register file");
    }

    pub fn completion_debug_info(&self) {
        tracing::debug!(
            "submitted_count: {}",
            self.user_data_counter.load(Ordering::Relaxed)
        );
        tracing::debug!("completed_count: {}", self.completed_count.get());
    }

    pub fn wait_cqueue(&self) -> bool {
        let ring = self.ring.borrow_mut();
        // 完了数が発行数より少ない場合にio_uring_enterを実行
        let completed_count = self.completed_count.get();
        let submitted_count = self.user_data_counter.load(Ordering::Relaxed);
        if completed_count >= submitted_count {
            return false;
        }
        ring.submit_and_wait(0)
            .expect("Failed to wait for completion");
        true
    }

    pub fn completed_count(&self) -> u64 {
        self.completed_count.get()
    }
}

impl Reactor for IoUringReactor {
    fn poll(&self) {
        self.poll_submit_and_completions();
    }

    fn status(&self) -> ReactorStatus {
        // Runningになる条件は以下
        // 1. submission の長さが submit_depth を超えた場合
        // 2. submission が空でなく、前回の送信から wait_submit_timeout を超えた場合
        // 3. 完了していないI/Oがあり、前回のenterからwait_complete_timeoutを超えた場合
        let mut ring = self.ring.borrow_mut();
        let submission = ring.submission();

        let submit_elapsed = {
            let last = self.last_submit_time.borrow();
            last.elapsed()
        };

        let submission_len = submission.len();
        if submission_len >= self.io_uring_params.submit_depth as usize
            || (!submission.is_empty()
                && submit_elapsed >= self.io_uring_params.wait_submit_timeout)
        {
            return ReactorStatus::Running;
        }

        // 完了していないI/Oがあり、前回のenterからwait_complete_timeoutを超えた場合
        let completed_elapsed = {
            let last = self.last_submit_time.borrow();
            last.elapsed()
        };

        if self.completed_count.get() < self.user_data_counter.load(Ordering::Relaxed)
            && completed_elapsed >= self.io_uring_params.wait_complete_timeout
        {
            return ReactorStatus::Running;
        }
        // それ以外は停止状態
        ReactorStatus::Stopped
    }
}
