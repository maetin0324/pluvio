use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    pin::Pin,
    rc::Rc,
    sync::atomic::AtomicU64,
    task::{Context, Poll},
    time::Duration,
};

use io_uring::IoUring;
use pluvio_runtime::reactor::ReactorStatus;

use crate::{
    allocator::{FixedBuffer, FixedBufferAllocator},
    builder::IoUringReactorBuilder,
};

thread_local! {
    static IOURING_REACTOR: std::cell::OnceCell<Rc<IoUringReactor>> = std::cell::OnceCell::new();
}

/// Reactor implementation using Linux `io_uring`.
pub struct IoUringReactor {
    pub ring: Rc<RefCell<IoUring>>,
    pub completions: Rc<RefCell<HashMap<u64, Rc<RefCell<HandleState<i32>>>>>>,
    pub user_data_counter: AtomicU64,
    pub allocator: Rc<FixedBufferAllocator>,
    pub(crate) last_submit_time: RefCell<std::time::Instant>,
    pub(crate) io_uring_params: IoUringParams,
    pub(crate) completed_count: Cell<u64>,
}

/// Parameters controlling io_uring behaviour.
pub(crate) struct IoUringParams {
    pub(crate) submit_depth: u32,
    pub(crate) wait_submit_timeout: Duration,
    pub(crate) wait_complete_timeout: Duration,
}

/// State shared between the reactor and a waiting future.
pub struct HandleState<T> {
    pub waker: RefCell<Option<std::task::Waker>>,
    pub result: RefCell<Option<std::io::Result<T>>>,
}
impl<T> HandleState<T> {
    /// Create a new empty [`HandleState`].
    pub fn new() -> Self {
        HandleState {
            waker: RefCell::new(None),
            result: RefCell::new(None),
        }
    }
}

/// Future returned from [`IoUringReactor::push_sqe`] used to wait on completion.
pub struct WaitHandle {
    pub handle_state: Rc<RefCell<HandleState<i32>>>,
}

impl WaitHandle {
    /// Create a new [`WaitHandle`] from the provided state.
    pub fn new(handle_state: Rc<RefCell<HandleState<i32>>>) -> Self {
        WaitHandle { handle_state }
    }
}

impl Future for WaitHandle {
    type Output = std::io::Result<i32>;

    /// Polls the handle waiting for the underlying I/O to finish.
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
    /// Create a reactor with default parameters.
    pub fn new() -> Rc<Self> {
        IoUringReactorBuilder::default().build()
    }

    pub fn init(reactor: Rc<Self>) -> Result<(), String> {
        IOURING_REACTOR.with(|cell| {
            cell.set(reactor)
                .map_err(|_| "Failed to set IoUringReactor in thread local storage".to_string())
        })
    }

    pub fn get_or_init() -> Rc<IoUringReactor> {
        IOURING_REACTOR.with(|cell| {
            let reactor_ref = cell.get_or_init(move || IoUringReactorBuilder::default().build());
            reactor_ref.clone()
        })
    }

    pub fn builder() -> IoUringReactorBuilder {
        IoUringReactorBuilder::new()
    }

    /// Push an SQE to the ring and return a [`WaitHandle`] for its completion.
    pub fn push_sqe(&self, sqe: io_uring::squeue::Entry) -> WaitHandle {
        let user_data = self
            .user_data_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    /// Submit all pending SQEs and process completions.
    pub fn poll_submit_and_completions(&self) {
        let mut ring = self.ring.borrow_mut();

        // SQE の送信
        ring.submit().expect("Failed to submit SQE");
        // ring.submit_and_wait(submission_len / 2).expect("Failed to submit SQE");
        // ring.submit_and_wait(submission_len).expect("Failed to submit SQE");

        let mut last = self.last_submit_time.borrow_mut();
        *last = std::time::Instant::now();

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
                        RefCell::new(Some(Err(std::io::Error::from_raw_os_error(-cqe.result()))));
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

    /// Acquire a fixed buffer from the internal allocator.
    #[async_backtrace::framed]
    pub async fn acquire_buffer(&self) -> FixedBuffer {
        self.allocator.acquire().await
    }

    /// Returns `true` if there are no pending completions.
    pub fn is_empty(&self) -> bool {
        let completions = self.completions.borrow_mut();
        completions.is_empty()
    }

    pub fn completion_debug_info(&self) {
        tracing::debug!(
            "submitted_count: {}",
            self.user_data_counter
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        tracing::debug!("completed_count: {}", self.completed_count.get());
    }

    /// Blockingly wait for at least one completion.
    pub fn wait_cqueue(&self) -> bool {
        let ring = self.ring.borrow_mut();
        // 完了数が発行数より少ない場合にio_uring_enterを実行
        let completed_count = self.completed_count.get();
        let submitted_count = self
            .user_data_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if completed_count >= submitted_count {
            return false;
        }
        ring.submit_and_wait(0)
            .expect("Failed to wait for completion");
        true
    }

    /// Number of completions processed by this reactor.
    pub fn completed_count(&self) -> u64 {
        self.completed_count.get()
    }
}

pub fn register_file(fd: i32) {
    let reactor = IoUringReactor::get_or_init();
    let ring = reactor.ring.borrow_mut();
    ring.submitter()
        .register_files(&[fd])
        .expect("Failed to register file");
}

impl pluvio_runtime::reactor::Reactor for IoUringReactor {
    fn poll(&self) {
        self.poll_submit_and_completions();
    }

    fn status(&self) -> pluvio_runtime::reactor::ReactorStatus {
        // Runningになる条件は以下
        // 1. submission の長さが submit_depth を超えた場合
        // 2. submission が空でなく、前回の送信から wait_submit_timeout を超えた場合
        // 3. 完了していないI/Oがあり、前回のenterからwait_complete_timeoutを超えた場合
        let mut ring = self.ring.borrow_mut();
        let submission = ring.submission();

        // elapsed()は1回だけ呼び出し、結果を再利用（clock_gettimeのオーバーヘッド削減）
        let elapsed = {
            let last = self.last_submit_time.borrow();
            last.elapsed()
        };

        let submission_len = submission.len();
        if submission_len >= self.io_uring_params.submit_depth as usize
            || (!submission.is_empty()
                && elapsed >= self.io_uring_params.wait_submit_timeout)
        {
            return ReactorStatus::Running;
        }

        // 完了していないI/Oがあり、前回のenterからwait_complete_timeoutを超えた場合
        if self.completed_count.get()
            < self
                .user_data_counter
                .load(std::sync::atomic::Ordering::Relaxed)
            && elapsed >= self.io_uring_params.wait_complete_timeout
        {
            return ReactorStatus::Running;
        }
        // それ以外は停止状態
        ReactorStatus::Stopped
    }
}
