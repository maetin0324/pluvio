//! `AllgatherFuture`: drives a single `MPI_Iallgather` to completion.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use mpi_sys::{MPI_Iallgather, MPI_Request, MPI_SUCCESS, RSMPI_COMM_WORLD as MPI_COMM_WORLD};

use crate::error::CollectiveError;
use crate::mpi_backend::datatype::MpiDatatype;
use crate::mpi_backend::reactor::{CompletionSlot, MpiReactor};

enum State {
    NotStarted,
    InFlight(Rc<CompletionSlot>),
    Done,
}

/// Future returned by `MpiCommunicator::allgather`.
pub struct AllgatherFuture<'a, T: MpiDatatype> {
    send_ptr: *const T,
    send_count: i32,
    recv_ptr: *mut T,
    recv_count: i32,
    reactor: Rc<MpiReactor>,
    state: State,
    _life: PhantomData<(&'a [T], &'a mut [T])>,
}

impl<'a, T: MpiDatatype> AllgatherFuture<'a, T> {
    pub(crate) fn new(
        reactor: Rc<MpiReactor>,
        send: &'a [T],
        recv: &'a mut [T],
        size: usize,
    ) -> Result<Self, CollectiveError> {
        if recv.len() != send.len() * size {
            return Err(CollectiveError::BadShape);
        }
        Ok(Self {
            send_ptr: send.as_ptr(),
            send_count: send.len() as i32,
            recv_ptr: recv.as_mut_ptr(),
            recv_count: send.len() as i32,
            reactor,
            state: State::NotStarted,
            _life: PhantomData,
        })
    }
}

impl<'a, T: MpiDatatype> Future for AllgatherFuture<'a, T> {
    type Output = Result<(), CollectiveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match &this.state {
            State::NotStarted => {
                let mut req = MPI_Request(std::ptr::null_mut());
                // SAFETY: send_ptr/recv_ptr are derived from references whose
                // lifetimes outlive the future via PhantomData. MPI reads
                // through send_ptr and writes through recv_ptr; the call is
                // non-blocking and we wait for completion via the reactor.
                let rc = unsafe {
                    MPI_Iallgather(
                        this.send_ptr as *const _,
                        this.send_count,
                        T::dtype(),
                        this.recv_ptr as *mut _,
                        this.recv_count,
                        T::dtype(),
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
