//! `BroadcastFuture`: drives a single `MPI_Ibcast` to completion.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use mpi_sys::{MPI_Ibcast, MPI_Request, MPI_SUCCESS, RSMPI_COMM_WORLD as MPI_COMM_WORLD};

use crate::error::CollectiveError;
use crate::mpi_backend::datatype::MpiDatatype;
use crate::mpi_backend::reactor::{CompletionSlot, MpiReactor};

enum State {
    NotStarted,
    InFlight(Rc<CompletionSlot>),
    Done,
}

/// Future returned by `MpiCommunicator::broadcast`.
pub struct BroadcastFuture<'a, T: MpiDatatype> {
    buf_ptr: *mut T,
    count: i32,
    root: i32,
    reactor: Rc<MpiReactor>,
    state: State,
    _life: PhantomData<&'a mut [T]>,
}

impl<'a, T: MpiDatatype> BroadcastFuture<'a, T> {
    pub(crate) fn new(reactor: Rc<MpiReactor>, buf: &'a mut [T], root: usize) -> Self {
        Self {
            buf_ptr: buf.as_mut_ptr(),
            count: buf.len() as i32,
            root: root as i32,
            reactor,
            state: State::NotStarted,
            _life: PhantomData,
        }
    }
}

impl<'a, T: MpiDatatype> Future for BroadcastFuture<'a, T> {
    type Output = Result<(), CollectiveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match &this.state {
            State::NotStarted => {
                let mut req = MPI_Request(std::ptr::null_mut());
                // SAFETY: buf_ptr is borrowed for `'a` (PhantomData), and the
                // request is awaited until completion before the future
                // resolves.
                let rc = unsafe {
                    MPI_Ibcast(
                        this.buf_ptr as *mut _,
                        this.count,
                        T::dtype(),
                        this.root,
                        MPI_COMM_WORLD,
                        &mut req,
                    )
                };
                if rc != MPI_SUCCESS as i32 {
                    return Poll::Ready(Err(CollectiveError::Mpi(rc)));
                }
                let slot = this.reactor.register(req, cx.waker().clone());
                this.state = State::InFlight(slot);
                Poll::Pending
            }
            State::InFlight(slot) => {
                if slot.done.get() {
                    this.state = State::Done;
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            State::Done => Poll::Ready(Ok(())),
        }
    }
}
