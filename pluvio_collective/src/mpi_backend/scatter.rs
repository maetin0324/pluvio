//! `ScatterFuture`: drives a single `MPI_Iscatter` to completion.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use mpi_sys::{MPI_Iscatter, MPI_Request, MPI_SUCCESS, RSMPI_COMM_WORLD as MPI_COMM_WORLD};

use crate::error::CollectiveError;
use crate::mpi_backend::datatype::MpiDatatype;
use crate::mpi_backend::reactor::{CompletionSlot, MpiReactor};

enum State {
    NotStarted,
    InFlight(Rc<CompletionSlot>),
    Done,
}

/// Future returned by `MpiCommunicator::scatter`.
///
/// At rank `root`, `send_ptr`/`send_len` describe the full input buffer (size
/// `recv_len * comm_size`); at non-root ranks they are `null`/`0` and MPI
/// ignores them.
pub struct ScatterFuture<'a, T: MpiDatatype> {
    send_ptr: *const T,
    send_count: i32,
    recv_ptr: *mut T,
    recv_count: i32,
    root: i32,
    reactor: Rc<MpiReactor>,
    state: State,
    _life: PhantomData<(&'a [T], &'a mut [T])>,
}

impl<'a, T: MpiDatatype> ScatterFuture<'a, T> {
    pub(crate) fn new(
        reactor: Rc<MpiReactor>,
        send: Option<&'a [T]>,
        recv: &'a mut [T],
        root: usize,
        rank: usize,
        size: usize,
    ) -> Result<Self, CollectiveError> {
        let recv_count = recv.len() as i32;
        let (send_ptr, send_count) = if rank == root {
            let sb = send.ok_or(CollectiveError::Protocol(
                "scatter: send_buf required at root",
            ))?;
            if sb.len() != recv.len() * size {
                return Err(CollectiveError::BadShape);
            }
            (sb.as_ptr(), recv_count)
        } else {
            (std::ptr::null(), 0)
        };
        Ok(Self {
            send_ptr,
            send_count,
            recv_ptr: recv.as_mut_ptr(),
            recv_count,
            root: root as i32,
            reactor,
            state: State::NotStarted,
            _life: PhantomData,
        })
    }
}

impl<'a, T: MpiDatatype> Future for ScatterFuture<'a, T> {
    type Output = Result<(), CollectiveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match &this.state {
            State::NotStarted => {
                let mut req = MPI_Request(std::ptr::null_mut());
                // SAFETY: at root, `send_ptr` was constructed from a `&[T]`
                // borrowed for `'a`; at non-root it is null and MPI ignores
                // it (sendcount = 0). `recv_ptr` came from `&mut [T]` borrowed
                // for `'a`. Both buffers stay live through the in-flight
                // request via the future's PhantomData borrow.
                let rc = unsafe {
                    MPI_Iscatter(
                        this.send_ptr as *const _,
                        this.send_count,
                        T::dtype(),
                        this.recv_ptr as *mut _,
                        this.recv_count,
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
