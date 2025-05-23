use std::{
    cell::{RefCell, Cell}, collections::HashMap, io::Error, rc::Rc, sync::atomic::{AtomicU64, Ordering}, time::{Duration, Instant}
};

use io_uring::IoUring;

use crate::io::HandleState;

pub struct Reactor {
    pub ring: Rc<RefCell<IoUring>>,
    pub completions: Rc<RefCell<HashMap<u64, Rc<RefCell<HandleState<usize>>>>>>,
    pub user_data_counter: AtomicU64,
    last_submit_time: RefCell<std::time::Instant>,
    io_uring_params: IoUringParams,
    completed_count: Cell<u64>,
}

struct IoUringParams {
    submit_depth: u32,
    wait_timeout: Duration,
}

impl Reactor {
    pub fn new(queue_size: u32, submit_depth: u32, wait_timeout: Duration) -> Self {
        // let ring = IoUring::new(queue_size).expect("Failed to create io_uring");
        let ring = IoUring::builder()
            // .setup_iopoll()
            .build(queue_size)
            .expect("Failed to create io_uring");
        if ring.params().is_feature_nodrop() {
            tracing::trace!("io_uring supports IORING_FEAT_NODROP");
        } else {
            tracing::trace!("io_uring does not support IORING_FEAT_NODROP");
        }
        Reactor {
            ring: Rc::new(RefCell::new(ring)),
            completions: Rc::new(RefCell::new(HashMap::new())),
            user_data_counter: AtomicU64::new(0),
            last_submit_time: RefCell::new(std::time::Instant::now()),
            io_uring_params: IoUringParams {
                submit_depth,
                wait_timeout,
            },
            completed_count: Cell::new(0),
        }
    }

    /// I/O 操作を登録し、対応する SharedState をマッピングに追加します。
    pub fn register_io(&self, handle_state: Rc<RefCell<HandleState<usize>>>) -> u64 {
        let user_data = self.user_data_counter.fetch_add(1, Ordering::Relaxed);
        let mut completions = self.completions.borrow_mut();
        completions.insert(user_data, handle_state);
        user_data
    }

    /// I/O 操作を送信します。
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

    /// 完了キューをポーリングし、完了した I/O 操作を処理します。
    pub fn poll_submit_and_completions(&self) {
        let mut ring = self.ring.borrow_mut();
        let submission = ring.submission();

        // SQE の送信を行うかどうかの判定
        // 1. submission の長さが submit_depth を超えた場合
        // 2. submission が空でなく、前回の送信から wait_timeout を超えた場合
        let elapsed = {
            let last = self.last_submit_time.borrow();
            last.elapsed()
        };

        let submission_len = submission.len();
        if submission_len >= self.io_uring_params.submit_depth as usize
            || (!submission.is_empty() && elapsed >= self.io_uring_params.wait_timeout)
        {
            tracing::trace!("submitted {} SQEs", submission.len());
            drop(submission);
            // SQE の送信
            ring.submit().expect("Failed to submit SQE");
            // ring.submit_and_wait(submission_len / 2).expect("Failed to submit SQE");
            // ring.submit_and_wait(submission_len).expect("Failed to submit SQE");

            let mut last = self.last_submit_time.borrow_mut();
            *last = Instant::now();
        } else {
            drop(submission);
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
                    handle.result = RefCell::new(Some(Ok(cqe.result() as usize)));
                } else {
                    // エラーハンドリング
                    handle.result = RefCell::new(Some(Err(
                        Error::from_raw_os_error(-cqe.result()).to_string()
                    )));
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

    pub fn is_empty(&self) -> bool {
        let completions = self.completions.borrow_mut();
        completions.is_empty()
    }

    pub fn register_file(&self, fd: i32) {
        let ring = self.ring.borrow_mut();
        ring.submitter().register_files(&[fd]).expect("Failed to register file");
    }

    pub fn completion_debug_info(&self) {
        tracing::debug!("submitted_count: {}", self.user_data_counter.load(Ordering::Relaxed));
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
        ring.submit_and_wait(0).expect("Failed to wait for completion");
        true
    }

    pub fn completed_count(&self) -> u64 {
        self.completed_count.get()
    }
}
