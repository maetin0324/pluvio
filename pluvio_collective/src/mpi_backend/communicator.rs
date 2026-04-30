//! `MpiCommunicator`: thin wrapper around `MPI_COMM_WORLD`.

use std::rc::Rc;

use mpi_sys::{MPI_Comm_rank, MPI_Comm_size, MPI_SUCCESS, RSMPI_COMM_WORLD as MPI_COMM_WORLD};

use crate::communicator::Communicator;
use crate::error::CollectiveError;
use crate::mpi_backend::allgather::AllgatherFuture;
use crate::mpi_backend::allreduce::AllreduceFuture;
use crate::mpi_backend::broadcast::BroadcastFuture;
use crate::mpi_backend::datatype::MpiDatatype;
use crate::mpi_backend::reactor::MpiReactor;
use crate::mpi_backend::scatter::ScatterFuture;
use crate::op::Op;
use crate::Collective;
use std::future::Future;
use std::pin::Pin;

/// MPI-backed communicator over `MPI_COMM_WORLD`.
///
/// `MPI_Init_thread` (or `MPI_Init`) must have been called before constructing
/// this, and `MPI_Finalize` is the caller's responsibility (typically at
/// program exit).
pub struct MpiCommunicator {
    rank: usize,
    size: usize,
    reactor: Rc<MpiReactor>,
}

impl MpiCommunicator {
    /// Build a communicator for `MPI_COMM_WORLD`. The caller must have
    /// initialised MPI; this only queries `rank` and `size`.
    pub fn world(reactor: Rc<MpiReactor>) -> Result<Self, CollectiveError> {
        let mut rank: i32 = 0;
        let mut size: i32 = 0;
        // SAFETY: MPI is assumed initialised. Both calls only read state from
        // MPI_COMM_WORLD and write to local stack variables.
        unsafe {
            let rc = MPI_Comm_rank(MPI_COMM_WORLD, &mut rank);
            if rc != MPI_SUCCESS as i32 {
                return Err(CollectiveError::Mpi(rc));
            }
            let rc = MPI_Comm_size(MPI_COMM_WORLD, &mut size);
            if rc != MPI_SUCCESS as i32 {
                return Err(CollectiveError::Mpi(rc));
            }
        }
        Ok(Self {
            rank: rank as usize,
            size: size as usize,
            reactor,
        })
    }

    /// Access the underlying reactor (used by examples that want to drive the
    /// runtime explicitly).
    pub fn reactor(&self) -> &Rc<MpiReactor> {
        &self.reactor
    }
}

impl Communicator for MpiCommunicator {
    fn rank(&self) -> usize {
        self.rank
    }
    fn size(&self) -> usize {
        self.size
    }
}

impl<T: MpiDatatype> Collective<T> for MpiCommunicator {
    type AllreduceFut<'a>
        = AllreduceFuture<'a, T>
    where
        Self: 'a,
        T: 'a;

    type ScatterFut<'a>
        = Pin<Box<dyn Future<Output = Result<(), CollectiveError>> + 'a>>
    where
        Self: 'a,
        T: 'a;

    type AllgatherFut<'a>
        = Pin<Box<dyn Future<Output = Result<(), CollectiveError>> + 'a>>
    where
        Self: 'a,
        T: 'a;

    type BroadcastFut<'a>
        = BroadcastFuture<'a, T>
    where
        Self: 'a,
        T: 'a;

    fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a> {
        AllreduceFuture::new(self.reactor.clone(), buf, O::mpi_op())
    }

    fn scatter<'a>(
        &'a self,
        send_buf: Option<&'a [T]>,
        recv_buf: &'a mut [T],
        root: usize,
    ) -> Self::ScatterFut<'a> {
        let reactor = self.reactor.clone();
        let rank = self.rank;
        let size = self.size;
        Box::pin(async move {
            let fut = ScatterFuture::new(reactor, send_buf, recv_buf, root, rank, size)?;
            fut.await
        })
    }

    fn allgather<'a>(
        &'a self,
        send_buf: &'a [T],
        recv_buf: &'a mut [T],
    ) -> Self::AllgatherFut<'a> {
        let reactor = self.reactor.clone();
        let size = self.size;
        Box::pin(async move {
            let fut = AllgatherFuture::new(reactor, send_buf, recv_buf, size)?;
            fut.await
        })
    }

    fn broadcast<'a>(&'a self, buf: &'a mut [T], root: usize) -> Self::BroadcastFut<'a> {
        BroadcastFuture::new(self.reactor.clone(), buf, root)
    }
}
