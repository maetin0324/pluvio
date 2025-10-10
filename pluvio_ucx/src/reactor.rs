use std::{cell::RefCell, rc::Rc};

use slab::Slab;

use crate::worker::Worker;
use pluvio_runtime::reactor::ReactorStatus;

thread_local! {
    pub static PLUVIO_UCX_REACTOR: std::cell::OnceCell<Rc<UCXReactor>> = std::cell::OnceCell::new();
}

pub struct UCXReactor {
    registered_workers: RefCell<Slab<Rc<Worker>>>,
}

impl UCXReactor {
    pub fn new() -> Self {
        Self {
            registered_workers: RefCell::new(Slab::new()),
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
        self.registered_workers
            .borrow()
            .iter()
            .any(|(_, worker)| worker.state() != crate::worker::WorkerState::Inactive)
            .then(|| ReactorStatus::Running)
            .unwrap_or(ReactorStatus::Stopped)
    }

    fn poll(&self) {
        for (_, worker) in self.registered_workers.borrow().iter() {
            // tracing::trace!("UCXReactor: polling worker");
            worker.inner().progress();
        }
    }
}
