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
/// Phase 1+2 currently exposes `Allreduce` and `Scatter`. Future phases
/// (broadcast, allgather, reduce_scatter, alltoall) will extend this trait.
pub trait Collective<T>: Communicator {
    type AllreduceFut<'a>: Future<Output = Result<(), CollectiveError>>
    where
        Self: 'a,
        T: 'a;

    type ScatterFut<'a>: Future<Output = Result<(), CollectiveError>>
    where
        Self: 'a,
        T: 'a;

    type AllgatherFut<'a>: Future<Output = Result<(), CollectiveError>>
    where
        Self: 'a,
        T: 'a;

    type BroadcastFut<'a>: Future<Output = Result<(), CollectiveError>>
    where
        Self: 'a,
        T: 'a;

    /// In-place allreduce: every rank ends up with the reduced value of `buf`
    /// across all ranks. The reduction operator is selected via the type
    /// parameter `O`.
    fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a>;

    /// Scatter the `send_buf` held by `root` into per-rank `recv_buf` chunks.
    ///
    /// At rank `root`, `send_buf` must be `Some(&[T])` of length
    /// `recv_buf.len() * size()` — the buffer is divided into `size()` equal
    /// chunks, and the chunk at index `r` is delivered to rank `r`. At every
    /// other rank `send_buf` is ignored and may be `None`.
    fn scatter<'a>(
        &'a self,
        send_buf: Option<&'a [T]>,
        recv_buf: &'a mut [T],
        root: usize,
    ) -> Self::ScatterFut<'a>;

    /// Allgather: each rank contributes `send_buf` of length `L`; on return
    /// every rank has `recv_buf[r * L .. (r+1) * L] = sender(r)`. `recv_buf`
    /// must therefore be of length `L * size()`.
    fn allgather<'a>(
        &'a self,
        send_buf: &'a [T],
        recv_buf: &'a mut [T],
    ) -> Self::AllgatherFut<'a>;

    /// Broadcast: rank `root`'s `buf` is delivered to every other rank's
    /// `buf`. At ranks other than `root`, `buf`'s incoming contents are
    /// overwritten.
    fn broadcast<'a>(&'a self, buf: &'a mut [T], root: usize) -> Self::BroadcastFut<'a>;
}
