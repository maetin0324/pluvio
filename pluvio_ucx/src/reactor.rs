#![allow(dead_code)]
use std::cell::{RefCell, UnsafeCell};
use std::rc::Rc;

use slab::Slab;

use crate::worker::Worker;
use pluvio_runtime::reactor::ReactorStatus;

thread_local! {
    pub static PLUVIO_UCX_REACTOR: std::cell::OnceCell<Rc<UCXReactor>> = std::cell::OnceCell::new();
}

pub struct UCXReactor {
    registered_workers: RefCell<Slab<Rc<Worker>>>,
    last_polled: RefCell<std::time::Instant>,
    connection_timeout: std::time::Duration,
    /// Counter for active/waiting workers to avoid iteration in status()
    /// This is called very frequently (313K+ times in benchmarks)
    active_or_waiting_count: UnsafeCell<usize>,
    /// Rndv operation counter for blocking mode
    rndv_count: UnsafeCell<usize>,
    /// Threshold for triggering blocking mode
    rndv_blocking_threshold: usize,
    /// Start time of first Rndv operation (for timeout-based blocking)
    rndv_start_time: UnsafeCell<Option<std::time::Instant>>,
    /// Timeout before triggering blocking mode
    rndv_blocking_timeout: std::time::Duration,
}

impl UCXReactor {
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Self {
        Self::with_rndv_config(
            Self::default_rndv_threshold(),
            Self::default_rndv_timeout(),
        )
    }

    pub fn with_rndv_config(threshold: usize, timeout: std::time::Duration) -> Self {
        // Read connection timeout from environment variable or use default of 30 seconds
        let timeout_secs = std::env::var("PLUVIO_CONNECTION_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        Self {
            registered_workers: RefCell::new(Slab::new()),
            last_polled: RefCell::new(std::time::Instant::now()),
            connection_timeout: std::time::Duration::from_secs(timeout_secs),
            active_or_waiting_count: UnsafeCell::new(0),
            rndv_count: UnsafeCell::new(0),
            rndv_blocking_threshold: threshold,
            rndv_start_time: UnsafeCell::new(None),
            rndv_blocking_timeout: timeout,
        }
    }

    fn default_rndv_threshold() -> usize {
        std::env::var("PLUVIO_RNDV_BLOCKING_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32)
    }

    fn default_rndv_timeout() -> std::time::Duration {
        let ms = std::env::var("PLUVIO_RNDV_BLOCKING_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10);
        std::time::Duration::from_millis(ms)
    }

    #[tracing::instrument(level = "trace")]
    pub fn current() -> Rc<Self> {
        PLUVIO_UCX_REACTOR.with(|cell| cell.get_or_init(|| Rc::new(Self::new())).clone())
    }

    #[tracing::instrument(level = "trace", skip(self, worker))]
    pub fn register_worker(&self, worker: Rc<Worker>) -> usize {
        self.registered_workers.borrow_mut().insert(worker)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn unregister_worker(&self, id: usize) {
        self.registered_workers.borrow_mut().remove(id);
    }

    /// Increment the active/waiting worker count.
    /// Called when a worker transitions to Active or WaitConnect state.
    pub fn increment_active_count(&self) {
        // SAFETY: Single-threaded access via thread_local UCXReactor
        unsafe {
            *self.active_or_waiting_count.get() += 1;
        }
    }

    /// Decrement the active/waiting worker count.
    /// Called when a worker transitions from Active or WaitConnect to Inactive.
    pub fn decrement_active_count(&self) {
        // SAFETY: Single-threaded access via thread_local UCXReactor
        unsafe {
            *self.active_or_waiting_count.get() -= 1;
        }
    }

    /// Increment the Rndv operation count.
    /// Called when a Rndv send or receive operation starts.
    pub fn increment_rndv_count(&self) {
        // SAFETY: Single-threaded access via thread_local UCXReactor
        unsafe {
            let count = self.rndv_count.get();
            let prev = *count;
            *count = prev + 1;
            if prev == 0 {
                *self.rndv_start_time.get() = Some(std::time::Instant::now());
            }
        }
    }

    /// Decrement the Rndv operation count.
    /// Called when a Rndv send or receive operation completes.
    pub fn decrement_rndv_count(&self) {
        // SAFETY: Single-threaded access via thread_local UCXReactor
        unsafe {
            let count = self.rndv_count.get();
            let prev = *count;
            *count = prev - 1;
            if prev == 1 {
                *self.rndv_start_time.get() = None;
            }
        }
    }

    /// Check if the reactor should enter blocking mode.
    fn should_block(&self) -> bool {
        // SAFETY: Single-threaded access via thread_local UCXReactor
        unsafe {
            let count = *self.rndv_count.get();
            if count == 0 {
                return false;
            }

            // Block if count exceeds threshold
            if count >= self.rndv_blocking_threshold {
                return true;
            }

            // Block if timeout has elapsed since first Rndv operation
            if let Some(start) = *self.rndv_start_time.get() {
                if start.elapsed() >= self.rndv_blocking_timeout {
                    return true;
                }
            }

            false
        }
    }
}

impl pluvio_runtime::reactor::Reactor for UCXReactor {
    fn status(&self) -> ReactorStatus {
        // Check for blocking mode first (Rndv operations in progress)
        if self.should_block() {
            return ReactorStatus::Blocking;
        }

        // Use UnsafeCell counter to avoid RefCell borrow and worker iteration overhead.
        // This method is called very frequently (313K+ times in benchmarks),
        // so avoiding the RefCell borrow and iteration is critical for performance.
        // SAFETY: Single-threaded access via thread_local UCXReactor
        if unsafe { *self.active_or_waiting_count.get() } > 0 {
            ReactorStatus::Running
        } else {
            ReactorStatus::Stopped
        }
    }

    fn poll(&self) {
        let workers = self.registered_workers.borrow();
        for (_, worker) in workers.iter() {
            worker.inner().progress();
        }
    }
}
