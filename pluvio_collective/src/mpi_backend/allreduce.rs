//! `AllreduceFuture`: drives a single `MPI_Iallreduce` to completion.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use mpi_sys::{
    MPI_Iallreduce, MPI_Op, MPI_Request, MPI_SUCCESS, RSMPI_COMM_WORLD as MPI_COMM_WORLD,
};

use crate::error::CollectiveError;
use crate::mpi_backend::datatype::MpiDatatype;
use crate::mpi_backend::reactor::{CompletionSlot, MpiReactor};

enum State {
    NotStarted,
    InFlight(Rc<CompletionSlot>),
    Done,
}

/// Future returned by `MpiCommunicator::allreduce`.
pub struct AllreduceFuture<'a, T: MpiDatatype> {
    buf: *mut T,
    len: usize,
    op: MPI_Op,
    reactor: Rc<MpiReactor>,
    state: State,
    _life: PhantomData<&'a mut [T]>,
}

impl<'a, T: MpiDatatype> AllreduceFuture<'a, T> {
    pub(crate) fn new(reactor: Rc<MpiReactor>, buf: &'a mut [T], op: MPI_Op) -> Self {
        Self {
            buf: buf.as_mut_ptr(),
            len: buf.len(),
            op,
            reactor,
            state: State::NotStarted,
            _life: PhantomData,
        }
    }
}

impl<'a, T: MpiDatatype> Future for AllreduceFuture<'a, T> {
    type Output = Result<(), CollectiveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match &this.state {
            State::NotStarted => {
                let mut req = MPI_Request(std::ptr::null_mut());
                // OpenMPI / MPICH define MPI_IN_PLACE as `(void *) 1`, an
                // intentionally-dangling pointer whose value is recognised by
                // MPI as a sentinel.
                #[allow(clippy::manual_dangling_ptr)]
                let in_place: *const std::os::raw::c_void = 1usize as *const _;
                // SAFETY: `this.buf` and `this.len` came from a `&mut [T]`
                // borrowed for `'a`, which outlives the future via the
                // PhantomData. The slice is exclusively borrowed by this
                // future for the duration of the in-flight request, so MPI
                // may safely read/write through the pointer.
                let rc = unsafe {
                    MPI_Iallreduce(
                        in_place,
                        this.buf as *mut _,
                        this.len as i32,
                        T::dtype(),
                        this.op,
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
