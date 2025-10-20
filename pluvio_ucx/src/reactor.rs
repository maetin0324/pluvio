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
}

impl UCXReactor {
    pub fn new() -> Self {
        Self {
            registered_workers: RefCell::new(Slab::new()),
            last_polled: RefCell::new(std::time::Instant::now()),
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
        // 最適化版: bool::thenを避けて直接if-elseを使用
        let workers = self.registered_workers.borrow();
        for (_, worker) in workers.iter() {
            match worker.state() {
                crate::worker::WorkerState::Active | crate::worker::WorkerState::WaitConnect => {
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
