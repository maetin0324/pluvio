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
    pub(crate) submit_thread: Option<SubmitThread>,
}

/// Userspace submit-offload thread ("userspace SQPOLL").
///
/// Kernel SQPOLL is unusable on Sirius's RHEL 9.6 kernel — heavy
/// BenchFS load with SQPOLL freezes whole hosts (2026-06-11/12 bisect).
/// SQPOLL's real benefit for us is not saved syscalls but moving the
/// inline block-layer submission work out of the single-threaded
/// reactor. This thread reproduces that: the reactor publishes new
/// SQEs with `SubmissionQueue::sync()` (a fenced memory write, no
/// syscall) and signals this thread, which performs the actual
/// `io_uring_enter` — so the NVMe submission CPU cost lands here
/// instead of on the reactor thread.
///
/// Enabled via env `PLUVIO_URING_SUBMIT_THREAD=1`; ignored when SQPOLL
/// is active. The thread is detached and lives for the process.
pub(crate) struct SubmitThread {
    pub(crate) flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) thread: std::thread::Thread,
}

impl SubmitThread {
    pub(crate) fn spawn(ring_fd: std::os::unix::io::RawFd, sq_entries: u32) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = std::sync::Arc::new(AtomicBool::new(false));
        let f = flag.clone();
        // `PLUVIO_URING_SUBMIT_THREAD=spin` busy-waits instead of
        // parking. park/unpark costs a futex round-trip (~µs) per
        // signal, which V1 (28753, 2026-06-12) showed erases the gain
        // for page-cache reads that complete inline in io_uring_enter:
        // ior-hard-write +8% vs kernel SQPOLL but ior-hard-read -28%.
        // Spinning burns one core per daemon; server hosts run 4
        // daemons on 24 cores, so that is affordable.
        let spin = std::env::var("PLUVIO_URING_SUBMIT_THREAD")
            .map(|v| v == "spin")
            .unwrap_or(false);
        let handle = std::thread::Builder::new()
            .name("pluvio-uring-sub".into())
            .spawn(move || {
                loop {
                    while !f.swap(false, Ordering::Acquire) {
                        if spin {
                            std::hint::spin_loop();
                        } else {
                            std::thread::park();
                        }
                    }
                    // `to_submit` only caps how many published SQEs the
                    // kernel consumes; passing the SQ size drains all.
                    // Errors (EINTR/EAGAIN) are not retried here: the
                    // reactor re-signals on every poll while SQEs are
                    // pending, so a failed enter is retried naturally.
                    unsafe {
                        libc::syscall(
                            libc::SYS_io_uring_enter,
                            ring_fd as libc::c_long,
                            sq_entries as libc::c_long,
                            0_i64,
                            0_i64,
                            std::ptr::null::<libc::sigset_t>(),
                            0_i64,
                        );
                    }
                }
            })
            .expect("spawn pluvio-uring-sub thread");
        SubmitThread {
            flag,
            thread: handle.thread().clone(),
        }
    }

    /// True when `PLUVIO_URING_SUBMIT_THREAD` requests the offload.
    pub(crate) fn enabled_by_env() -> bool {
        std::env::var("PLUVIO_URING_SUBMIT_THREAD")
            .map(|v| v != "0")
            .unwrap_or(false)
    }
}

/// Parameters controlling io_uring behaviour.
pub(crate) struct IoUringParams {
    pub(crate) submit_depth: u32,
    pub(crate) wait_submit_timeout: Duration,
    pub(crate) wait_complete_timeout: Duration,
    pub(crate) sq_poll: Option<u32>,
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
            return Poll::Ready(result);
        }

        // Waker を登録
        handle.waker.replace(Some(cx.waker().clone()));
        Poll::Pending
    }
}

impl IoUringReactor {
    /// Create a reactor with default parameters.
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Rc<Self> {
        IoUringReactorBuilder::default().build()
    }

    #[tracing::instrument(level = "trace", skip(reactor))]
    pub fn init(reactor: Rc<Self>) -> Result<(), String> {
        IOURING_REACTOR.with(|cell| {
            cell.set(reactor)
                .map_err(|_| "Failed to set IoUringReactor in thread local storage".to_string())
        })
    }

    #[tracing::instrument(level = "trace")]
    pub fn get_or_init() -> Rc<IoUringReactor> {
        IOURING_REACTOR.with(|cell| {
            let reactor_ref = cell.get_or_init(move || IoUringReactorBuilder::default().build());
            reactor_ref.clone()
        })
    }

    #[tracing::instrument(level = "trace")]
    pub fn builder() -> IoUringReactorBuilder {
        IoUringReactorBuilder::new()
    }

    /// Push an SQE to the ring and return a [`WaitHandle`] for its completion.
    // tracing::instrument removed — hot path called 320k+ times/sec.
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
    ///
    /// Optimized to skip unnecessary syscalls:
    /// - Only calls submit() when there are pending SQEs
    /// - Early exits if no work to do
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn poll_submit_and_completions(&self) {
        let mut ring = self.ring.borrow_mut();

        // Check if there are pending SQEs to submit
        let pending_sqe_count = ring.submission().len();
        let has_pending_sqes = pending_sqe_count > 0;

        if self.io_uring_params.sq_poll.is_some() {
            // SQPOLLモードでも、カーネルSQスレッドが`sq_thread_idle`経過後に
            // sleepする。新しいSQEをpushしてもsleep中のスレッドは自動的に
            // 起きないので、`sq.need_wakeup()`をチェックして必要なら
            // `submit()`を呼んで`IORING_ENTER_SQ_WAKEUP`を発行する。
            // io_uring crateの`submit()`はSQPOLL有効時に内部で必要に応じ
            // wake-up flagを付与する。
            if has_pending_sqes && ring.submission().need_wakeup() {
                ring.submit().expect("Failed to submit SQE (SQPOLL wakeup)");
            }
        } else if has_pending_sqes {
            if let Some(st) = &self.submit_thread {
                // Userspace SQPOLL: publish the tail (memory write, no
                // syscall) and let the dedicated thread io_uring_enter.
                ring.submission().sync();
                st.flag.store(true, std::sync::atomic::Ordering::Release);
                st.thread.unpark();
            } else {
                // Only submit if there are pending SQEs (avoid unnecessary syscall)
                ring.submit().expect("Failed to submit SQE");
            }
        }

        // Update last submit time only if we actually had work
        if has_pending_sqes {
            let mut last = self.last_submit_time.borrow_mut();
            *last = std::time::Instant::now();
        }

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
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn acquire_buffer(&self) -> FixedBuffer {
        self.allocator.acquire().await
    }

    /// Returns `true` if there are no pending completions.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn is_empty(&self) -> bool {
        let completions = self.completions.borrow_mut();
        completions.is_empty()
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn completion_debug_info(&self) {
        tracing::debug!(
            "submitted_count: {}",
            self.user_data_counter
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        tracing::debug!("completed_count: {}", self.completed_count.get());
    }

    /// Blockingly wait for at least one completion.
    #[tracing::instrument(level = "trace", skip(self))]
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
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn completed_count(&self) -> u64 {
        self.completed_count.get()
    }
}

#[tracing::instrument(level = "trace")]
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
        // Two modes:
        //
        // 1. Timeout-gated (default): Running only when submit_depth is hit
        //    or a configured timeout (wait_submit_timeout / wait_complete_timeout)
        //    has elapsed. Stales CQEs by up to ~timeout × status_cache_iterations.
        //
        // 2. Active-poll (PLUVIO_URING_ALWAYS_POLL=1): Running whenever
        //    there's any pending SQE or in-flight I/O. Drains CQEs every
        //    runtime iteration. Burns more CPU at idle but lets a chunk-write
        //    completion wake the awaiting future within ~1 µs instead of
        //    waiting on the timeout (job 17067 profile showed write_us p99
        //    inflated from 55→1157 µs by the default 1 ms timeout).
        let mut ring = self.ring.borrow_mut();
        let submission = ring.submission();

        let completed_count = self.completed_count.get();
        let submitted_count = self
            .user_data_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        let has_pending_io = completed_count < submitted_count;
        let has_pending_sqe = !submission.is_empty();

        // Cached once per process via std::sync::OnceLock to avoid repeated
        // env::var() lookups in the hot status() path.
        use std::sync::OnceLock;
        static ALWAYS_POLL: OnceLock<bool> = OnceLock::new();
        let always_poll = *ALWAYS_POLL.get_or_init(|| {
            std::env::var("PLUVIO_URING_ALWAYS_POLL")
                .ok()
                .map(|v| v != "0")
                .unwrap_or(false)
        });

        if always_poll {
            if has_pending_sqe || has_pending_io {
                return ReactorStatus::Running;
            }
            return ReactorStatus::Stopped;
        }

        // elapsed()は1回だけ呼び出し、結果を再利用（clock_gettimeのオーバーヘッド削減）
        let elapsed = {
            let last = self.last_submit_time.borrow();
            last.elapsed()
        };

        if self.io_uring_params.sq_poll.is_some() {
            // SQPOLLモード: pending SQEがあり、カーネルSQスレッドが
            // sleep中（need_wakeup）の場合は起こす必要がある。
            // CQ側は通常通り complete_timeout チェック。
            if has_pending_sqe && submission.need_wakeup() {
                return ReactorStatus::Running;
            }
            if has_pending_io && elapsed >= self.io_uring_params.wait_complete_timeout {
                return ReactorStatus::Running;
            }
        } else {
            // SQPOLLモードでない場合、3条件をチェック
            let submission_len = submission.len();
            if submission_len >= self.io_uring_params.submit_depth as usize
                || (has_pending_sqe && elapsed >= self.io_uring_params.wait_submit_timeout)
                || (has_pending_io && elapsed >= self.io_uring_params.wait_complete_timeout)
            {
                return ReactorStatus::Running;
            }
        }

        // それ以外は停止状態
        ReactorStatus::Stopped
    }
}
