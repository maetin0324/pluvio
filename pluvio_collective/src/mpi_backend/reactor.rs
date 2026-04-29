//! `MpiReactor`: drives MPI non-blocking request completion via `MPI_Test`.
//!
//! Registered on the Pluvio `Runtime`, it is polled in lockstep with the rest
//! of the reactors. The `active` counter tracks in-flight `MPI_Request`s; while
//! it is non-zero, the runtime keeps spinning (so we don't park while a
//! collective is unfinished).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::Waker;

use mpi_sys::{MPI_Request, MPI_Status, MPI_SUCCESS, MPI_Test};
use pluvio_runtime::reactor::{Reactor, ReactorStatus};

/// State shared between an `AllreduceFuture` and the reactor: when `MPI_Test`
/// reports completion, the reactor flips `done` and wakes the waker.
pub(crate) struct CompletionSlot {
    pub done: Cell<bool>,
}

impl CompletionSlot {
    fn new() -> Self {
        Self {
            done: Cell::new(false),
        }
    }
}

struct PendingReq {
    req: MPI_Request,
    waker: Waker,
    slot: Rc<CompletionSlot>,
}

/// Reactor that drives MPI non-blocking request completion. Single-threaded
/// only (uses `Cell`/`RefCell`).
pub struct MpiReactor {
    pending: RefCell<Vec<PendingReq>>,
    /// Number of in-flight requests. Mirrors `pending.len()` but is read on the
    /// hot `status()` path so we keep it in a `Cell`.
    active: Cell<usize>,
}

impl MpiReactor {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            pending: RefCell::new(Vec::new()),
            active: Cell::new(0),
        })
    }

    /// Register an in-flight `MPI_Request` together with the future's waker.
    /// Returns a slot whose `done` flag will be set when the request completes.
    pub(crate) fn register(&self, req: MPI_Request, waker: Waker) -> Rc<CompletionSlot> {
        let slot = Rc::new(CompletionSlot::new());
        self.pending.borrow_mut().push(PendingReq {
            req,
            waker,
            slot: slot.clone(),
        });
        self.active.set(self.active.get() + 1);
        slot
    }
}

impl Reactor for MpiReactor {
    fn status(&self) -> ReactorStatus {
        if self.active.get() > 0 {
            ReactorStatus::Running
        } else {
            ReactorStatus::Stopped
        }
    }

    fn poll(&self) {
        let mut pending = self.pending.borrow_mut();
        let mut i = 0;
        while i < pending.len() {
            let mut flag: i32 = 0;
            let mut status: MPI_Status = unsafe { std::mem::zeroed() };
            // SAFETY: `pending[i].req` is a valid MPI_Request handle obtained
            // from a successful MPI non-blocking call. MPI_Test is safe to
            // call repeatedly on the same handle until it reports completion;
            // it is only invoked here from the executor thread (single-threaded).
            let rc = unsafe { MPI_Test(&mut pending[i].req, &mut flag, &mut status) };
            debug_assert_eq!(rc, MPI_SUCCESS as i32);
            if flag != 0 {
                let entry = pending.swap_remove(i);
                entry.slot.done.set(true);
                entry.waker.wake();
                self.active.set(self.active.get() - 1);
            } else {
                i += 1;
            }
        }
    }
}
