#![allow(dead_code)]
use std::{cell::RefCell, rc::Rc};

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
}

impl UCXReactor {
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
        }
    }

    pub fn current() -> Rc<Self> {
        PLUVIO_UCX_REACTOR.with(|cell| cell.get_or_init(|| Rc::new(Self::new())).clone())
    }

    pub fn register_worker(&self, worker: Rc<Worker>) -> usize {
        self.registered_workers.borrow_mut().insert(worker)
    }

    pub fn unregister_worker(&self, id: usize) {
        self.registered_workers.borrow_mut().remove(id);
    }
}

impl pluvio_runtime::reactor::Reactor for UCXReactor {
    fn status(&self) -> ReactorStatus {
        let workers = self.registered_workers.borrow();

        for (_, worker) in workers.iter() {
            match worker.state() {
                crate::worker::WorkerState::Active => {
                    return ReactorStatus::Running;
                }
                crate::worker::WorkerState::WaitConnect => {
                    // WaitConnect state during AM stream message waiting is a normal state.
                    // Treat it as Running to prevent spurious "Runtime stuck" errors.
                    return ReactorStatus::Running;
                }
                crate::worker::WorkerState::Inactive => {}
            }
        }
        ReactorStatus::Stopped
    }

    fn poll(&self) {
        let workers = self.registered_workers.borrow();
        for (_, worker) in workers.iter() {
            worker.inner().progress();
        }
    }
}
