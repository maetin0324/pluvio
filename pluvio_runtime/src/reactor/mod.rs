//! Reactor implementation built on `io_uring`.
//!
//! The reactor is responsible for submitting and completing I/O operations
//! and waking the associated tasks.

use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Current running state of a reactor.
pub enum ReactorStatus {
    /// Reactor is actively processing
    Running,
    /// Reactor has no pending work
    Stopped,
    /// Reactor requires exclusive access (other reactors should NOT be polled)
    Blocking,
}

/// Common interface for reactor implementations.
pub trait Reactor {
    /// Poll the reactor for I/O events.
    fn poll(&self);
    /// Retrieve the current [`ReactorStatus`].
    fn status(&self) -> ReactorStatus;
}

impl<R: Reactor> Reactor for Rc<R> {
    fn poll(&self) {
        self.as_ref().poll();
    }

    fn status(&self) -> ReactorStatus {
        self.as_ref().status()
    }
}
