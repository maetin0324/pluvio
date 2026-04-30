//! Adaptive algorithm-selection wrapper.
//!
//! `AdaptiveCommunicator` wraps a `UcxCommunicator` and an `AdaptivePolicy`,
//! and chooses the allreduce algorithm at call time based on message size and
//! rank count. The other collectives (scatter, allgather, broadcast) currently
//! have only one implementation each on the UCX side, so they're forwarded
//! verbatim.

use std::pin::Pin;
use std::rc::Rc;

use futures::FutureExt;

use crate::Collective;
use crate::communicator::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::communicator::UcxCommunicator;
use crate::ucx_backend::pipelined_ring::{PipelineConfig, pipelined_ring_allreduce_typed};
use crate::ucx_backend::recursive_doubling::recursive_doubling_typed;
use crate::ucx_backend::ring::ring_allreduce_typed;
use crate::ucx_backend::{broadcast, bruck, scatter};

/// Allreduce algorithm chosen by `AdaptivePolicy`.
#[derive(Copy, Clone, Debug)]
pub enum AllreduceAlgo {
    PlainRing,
    PipelinedRing { config: PipelineConfig },
    RecursiveDoubling,
}

/// Selection policy for adaptive allreduce. Default thresholds are tuned
/// against MPICH's internal cutovers and should be revisited per-cluster.
#[derive(Copy, Clone, Debug)]
pub struct AdaptivePolicy {
    /// Below this many message bytes (and on power-of-two `n`), use
    /// recursive doubling.
    pub small_threshold: usize,
    /// At/above this many message bytes, use pipelined ring with
    /// `pipeline_config`. Between `small_threshold` and `large_threshold`,
    /// plain ring is used.
    pub large_threshold: usize,
    /// Pipeline parameters used when the policy chooses `PipelinedRing`.
    pub pipeline_config: PipelineConfig,
}

impl Default for AdaptivePolicy {
    fn default() -> Self {
        Self {
            small_threshold: 2 * 1024,
            large_threshold: 64 * 1024,
            pipeline_config: PipelineConfig::default(),
        }
    }
}

impl AdaptivePolicy {
    pub fn choose_allreduce(&self, msg_bytes: usize, n: usize) -> AllreduceAlgo {
        if msg_bytes < self.small_threshold && n.is_power_of_two() && n >= 2 {
            AllreduceAlgo::RecursiveDoubling
        } else if msg_bytes >= self.large_threshold {
            AllreduceAlgo::PipelinedRing {
                config: self.pipeline_config,
            }
        } else {
            AllreduceAlgo::PlainRing
        }
    }
}

/// `UcxCommunicator` wrapper that picks an algorithm per call.
pub struct AdaptiveCommunicator {
    inner: Rc<UcxCommunicator>,
    policy: AdaptivePolicy,
}

impl AdaptiveCommunicator {
    pub fn new(inner: Rc<UcxCommunicator>, policy: AdaptivePolicy) -> Self {
        Self { inner, policy }
    }

    pub fn inner(&self) -> &Rc<UcxCommunicator> {
        &self.inner
    }

    pub fn policy(&self) -> &AdaptivePolicy {
        &self.policy
    }

    /// Inspect which algorithm would be chosen for a hypothetical call. Used
    /// in tests and examples that want to assert the cutover boundaries.
    pub fn chosen_algo<T>(&self, len: usize) -> AllreduceAlgo {
        let bytes = len * std::mem::size_of::<T>();
        self.policy.choose_allreduce(bytes, self.inner.size())
    }
}

impl Communicator for AdaptiveCommunicator {
    fn rank(&self) -> usize {
        self.inner.rank()
    }
    fn size(&self) -> usize {
        self.inner.size()
    }
}

type BoxedFut<'a> = Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>;

impl<T> Collective<T> for AdaptiveCommunicator
where
    T: Copy + Default + bytemuck::Pod + 'static,
{
    type AllreduceFut<'a>
        = BoxedFut<'a>
    where
        Self: 'a,
        T: 'a;

    type ScatterFut<'a>
        = BoxedFut<'a>
    where
        Self: 'a,
        T: 'a;

    type AllgatherFut<'a>
        = BoxedFut<'a>
    where
        Self: 'a,
        T: 'a;

    type BroadcastFut<'a>
        = BoxedFut<'a>
    where
        Self: 'a,
        T: 'a;

    fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a> {
        let bytes = std::mem::size_of_val(buf);
        let algo = self.policy.choose_allreduce(bytes, self.inner.size());
        let inner = self.inner.clone();
        match algo {
            AllreduceAlgo::PlainRing => async move {
                ring_allreduce_typed::<T, O>(&inner, buf).await
            }
            .boxed_local(),
            AllreduceAlgo::PipelinedRing { config } => async move {
                pipelined_ring_allreduce_typed::<T, O>(&inner, buf, config).await
            }
            .boxed_local(),
            AllreduceAlgo::RecursiveDoubling => async move {
                recursive_doubling_typed::<T, O>(&inner, buf).await
            }
            .boxed_local(),
        }
    }

    fn scatter<'a>(
        &'a self,
        send_buf: Option<&'a [T]>,
        recv_buf: &'a mut [T],
        root: usize,
    ) -> Self::ScatterFut<'a> {
        let inner = self.inner.clone();
        async move { scatter::scatter_typed::<T>(&inner, send_buf, recv_buf, root).await }
            .boxed_local()
    }

    fn allgather<'a>(
        &'a self,
        send_buf: &'a [T],
        recv_buf: &'a mut [T],
    ) -> Self::AllgatherFut<'a> {
        let inner = self.inner.clone();
        async move { bruck::bruck_allgather_typed::<T>(&inner, send_buf, recv_buf).await }
            .boxed_local()
    }

    fn broadcast<'a>(&'a self, buf: &'a mut [T], root: usize) -> Self::BroadcastFut<'a> {
        let inner = self.inner.clone();
        async move { broadcast::broadcast_typed::<T>(&inner, buf, root).await }.boxed_local()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(policy: &AdaptivePolicy, bytes: usize, n: usize) -> AllreduceAlgo {
        policy.choose_allreduce(bytes, n)
    }

    #[test]
    fn policy_small_pow2_chooses_rd() {
        let p = AdaptivePolicy::default();
        assert!(matches!(
            pick(&p, 256, 4),
            AllreduceAlgo::RecursiveDoubling
        ));
    }

    #[test]
    fn policy_small_non_pow2_falls_back_to_ring() {
        let p = AdaptivePolicy::default();
        assert!(matches!(pick(&p, 256, 3), AllreduceAlgo::PlainRing));
    }

    #[test]
    fn policy_medium_uses_plain_ring() {
        let p = AdaptivePolicy::default();
        assert!(matches!(
            pick(&p, 8 * 1024, 4),
            AllreduceAlgo::PlainRing
        ));
    }

    #[test]
    fn policy_large_uses_pipelined() {
        let p = AdaptivePolicy::default();
        assert!(matches!(
            pick(&p, 1 << 20, 4),
            AllreduceAlgo::PipelinedRing { .. }
        ));
    }
}
