//! Pluvio collective communication primitives.
//!
//! Phase 1+2 minimal implementation: `Allreduce` only, with two backends:
//! - `mpi_backend`: wraps `MPI_Iallreduce` via `mpi-sys` and a custom reactor.
//! - `ucx_backend`: ring allreduce on top of `pluvio_ucx` Active Messages.
//!
//! See `docs/pluvio_collective_plan.md` for the design and scope.

pub mod communicator;
pub mod error;
pub mod op;

#[cfg(feature = "mpi")]
pub mod mpi_backend;

#[cfg(feature = "ucx")]
pub mod ucx_backend;

pub use communicator::Communicator;
pub use error::CollectiveError;
pub use op::{BitXor, Max, Min, Op, Prod, Sum};

use std::future::Future;

/// Collective operations supported by both backends.
///
/// Phase 1+2 only exposes `Allreduce`. Future phases (broadcast, allgather,
/// reduce_scatter, alltoall) will extend this trait.
pub trait Collective<T>: Communicator {
    type AllreduceFut<'a>: Future<Output = Result<(), CollectiveError>>
    where
        Self: 'a,
        T: 'a;

    /// In-place allreduce: every rank ends up with the reduced value of `buf`
    /// across all ranks. The reduction operator is selected via the type
    /// parameter `O`.
    fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a>;
}
