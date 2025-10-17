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
        // ReactorStatusは以下の順に決定する
        // 1. ActiveなWorkerが1つ以上ある場合はRunning
        // 2. ActiveなWorkerがなく、WaitConnectなWorkerが1つ以上ある場合は10msごとにRunning
        // 3. それ以外はStopped
        // let mut has_active = false;
        // let mut has_wait_connect = false;
        // for (_, worker) in self.registered_workers.borrow().iter() {
        //     match worker.state() {
        //         crate::worker::WorkerState::Active => {
        //             has_active = true;
        //             break;
        //         }
        //         crate::worker::WorkerState::WaitConnect => {
        //             has_wait_connect = true;
        //         }
        //         crate::worker::WorkerState::Inactive => {}
        //     }
        // }
        // if has_active {
        //     self.last_polled.replace(std::time::Instant::now());
        //     ReactorStatus::Running
        // } else if has_wait_connect {
        //     let now = std::time::Instant::now();
        //     let last_polled = *self.last_polled.borrow();
        //     if now.duration_since(last_polled).as_millis() >= 10 {
        //         self.last_polled.replace(now);
        //         tracing::debug!("UCXReactor: polling for WaitConnect workers");
        //         ReactorStatus::Running
        //     } else {
        //         ReactorStatus::Stopped
        //     }
        // } else {
        //     ReactorStatus::Stopped
        // }

        self.registered_workers
            .borrow()
            .iter()
            .any(|(_, worker)| {
                matches!(
                    worker.state(),
                    crate::worker::WorkerState::Active | crate::worker::WorkerState::WaitConnect
                )
            })
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
