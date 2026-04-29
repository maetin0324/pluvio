//! `UcxCommunicator`: a fixed group of UCX endpoints with a registered AM stream.

use std::rc::Rc;

use pluvio_ucx::Worker;
use pluvio_ucx::worker::endpoint::Endpoint;

use crate::Collective;
use crate::communicator::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::am_router::AmRouter;
use crate::ucx_backend::ring::{RingAllreduceFuture, ring_allreduce_typed};

/// AM id used by the collective layer. We use a single id and disambiguate
/// via the header (src, step, phase). For multiple concurrent communicators a
/// future revision can take this as a constructor parameter.
pub const COLLECTIVE_AM_ID: u16 = 0xC011;

/// Communicator that drives ring allreduce over UCX Active Messages.
pub struct UcxCommunicator {
    rank: usize,
    size: usize,
    endpoints: Vec<Option<Rc<Endpoint>>>,
    worker: Rc<Worker>,
    router: Rc<AmRouter>,
}

impl UcxCommunicator {
    pub fn new(
        rank: usize,
        size: usize,
        worker: Rc<Worker>,
        endpoints: Vec<Option<Rc<Endpoint>>>,
        router: Rc<AmRouter>,
    ) -> Self {
        Self {
            rank,
            size,
            endpoints,
            worker,
            router,
        }
    }

    pub(crate) fn router(&self) -> &Rc<AmRouter> {
        &self.router
    }

    pub(crate) fn endpoint(&self, rank: usize) -> Option<&Rc<Endpoint>> {
        self.endpoints.get(rank).and_then(|e| e.as_ref())
    }

    pub fn worker(&self) -> &Rc<Worker> {
        &self.worker
    }
}

impl Communicator for UcxCommunicator {
    fn rank(&self) -> usize {
        self.rank
    }
    fn size(&self) -> usize {
        self.size
    }
}

impl<T> Collective<T> for UcxCommunicator
where
    T: Copy + Default + bytemuck::Pod + 'static,
{
    type AllreduceFut<'a>
        = RingAllreduceFuture<'a, T>
    where
        Self: 'a,
        T: 'a;

    fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a> {
        ring_allreduce_typed::<T, O>(self, buf)
    }
}

impl UcxCommunicator {
    /// Convenience for explicit error handling without going through the
    /// `Collective` trait.
    pub async fn allreduce_with<T, O>(&self, buf: &mut [T]) -> Result<(), CollectiveError>
    where
        T: Copy + Default + bytemuck::Pod + 'static,
        O: Op<T>,
    {
        ring_allreduce_typed::<T, O>(self, buf).await
    }
}
