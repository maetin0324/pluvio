#![allow(dead_code)]
use std::{cell::RefCell, rc::Rc};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    /// Atomic counter for active/waiting workers to avoid iteration in status()
    /// This is called very frequently (313K+ times in benchmarks)
    active_or_waiting_count: AtomicUsize,
}

impl UCXReactor {
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Self {
        // Read timeout from environment variable or use default of 30 seconds
        let timeout_secs = std::env::var("PLUVIO_CONNECTION_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        Self {
            registered_workers: RefCell::new(Slab::new()),
            last_polled: RefCell::new(std::time::Instant::now()),
            connection_timeout: std::time::Duration::from_secs(timeout_secs),
            active_or_waiting_count: AtomicUsize::new(0),
        }
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
        self.active_or_waiting_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the active/waiting worker count.
    /// Called when a worker transitions from Active or WaitConnect to Inactive.
    pub fn decrement_active_count(&self) {
        self.active_or_waiting_count.fetch_sub(1, Ordering::Relaxed);
    }
}

impl pluvio_runtime::reactor::Reactor for UCXReactor {
    fn status(&self) -> ReactorStatus {
        // Use atomic counter to avoid RefCell borrow and worker iteration overhead.
        // This method is called very frequently (313K+ times in benchmarks),
        // so avoiding the RefCell borrow and iteration is critical for performance.
        if self.active_or_waiting_count.load(Ordering::Relaxed) > 0 {
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
