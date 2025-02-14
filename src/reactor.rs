use std::{
    cell::RefCell,
    collections::HashMap,
    io::Error,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use io_uring::IoUring;

use crate::task::SharedState;

pub struct Reactor {
    pub ring: Arc<Mutex<IoUring>>,
    pub completions: Arc<Mutex<HashMap<u64, Arc<Mutex<SharedState<usize>>>>>>,
    pub user_data_counter: AtomicU64,
    last_submit_time: RefCell<std::time::Instant>,
    io_uring_params: IoUringParams,
}

struct IoUringParams {
    submit_depth: u32,
    wait_timeout: Duration,
}

impl Reactor {
    pub fn new(queue_size: u32, submit_depth: u32, wait_timeout: Duration) -> Self {
        // let ring = IoUring::new(queue_size).expect("Failed to create io_uring");
        let ring = IoUring::builder()
            .setup_iopoll()
            .build(queue_size)
            .expect("Failed to create io_uring");
        Reactor {
            ring: Arc::new(Mutex::new(ring)),
            completions: Arc::new(Mutex::new(HashMap::new())),
            user_data_counter: AtomicU64::new(1),
            last_submit_time: RefCell::new(std::time::Instant::now()),
            io_uring_params: IoUringParams {
                submit_depth,
                wait_timeout,
            },
        }
    }

    /// I/O 操作を登録し、対応する SharedState をマッピングに追加します。
    pub fn register_io(&self, shared_state: Arc<Mutex<SharedState<usize>>>) -> u64 {
        let user_data = self.user_data_counter.fetch_add(1, Ordering::Relaxed);
        let mut completions = self.completions.lock().unwrap();
        completions.insert(user_data, shared_state);
        user_data
    }

    /// I/O 操作を送信します。
    pub fn submit_io(&self, sqe: io_uring::squeue::Entry, user_data: u64) {
        tracing::trace!("Reactor::submit_io user_data: {}", user_data);
        let mut ring = self.ring.lock().unwrap();
        unsafe {
            let sqe = sqe.user_data(user_data);
            ring.submission()
                .push(&sqe)
                .expect("Submission queue is full");
        }
    }

    /// 完了キューをポーリングし、完了した I/O 操作を処理します。
    pub fn poll_submit_and_completions(&self) {
        let mut ring = self.ring.lock().unwrap();
        let submission = ring.submission();

        let elapsed = {
            let last = self.last_submit_time.borrow();
            last.elapsed()
        };

        if submission.len() >= self.io_uring_params.submit_depth as usize
            || (!submission.is_empty() && elapsed >= self.io_uring_params.wait_timeout)
        {
            tracing::debug!("submitted {} SQEs", submission.len());
            drop(submission);
            // SQE の送信
            ring.submit().expect("Failed to submit SQE");

            let mut last = self.last_submit_time.borrow_mut();
            *last = Instant::now();
        } else {
            drop(submission);
        }

        let cq = ring.completion();

        for cqe in cq {
            let user_data = cqe.user_data();

            // マッピングから SharedState を取得
            let shared_state_opt = {
                let mut completions = self.completions.lock().unwrap();
                completions.remove(&user_data)
            };

            if let Some(shared_state) = shared_state_opt {
                let mut shared = shared_state.lock().unwrap();
                if cqe.result() >= 0 {
                    shared.result = Mutex::new(Some(Ok(cqe.result() as usize)));
                } else {
                    // エラーハンドリング
                    shared.result = Mutex::new(Some(Err(
                        Error::from_raw_os_error(-cqe.result()).to_string()
                    )));
                }

                // Waker を呼び出してタスクを再開
                let waker_opt = shared.waker.lock().unwrap().take();
                if let Some(waker) = waker_opt {
                    waker.wake();
                }
            } else {
                eprintln!("Received completion for unknown user_data: {}", user_data);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        let completions = self.completions.lock().unwrap();
        completions.is_empty()
    }
}
