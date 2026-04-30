//! UCX backend (Phase 2): ring allreduce on top of pluvio_ucx Active Messages.

pub mod adaptive;
pub mod am_router;
pub mod bootstrap;
pub mod broadcast;
pub mod bruck;
pub mod communicator;
pub mod pipelined_ring;
pub mod recursive_doubling;
pub mod ring;
pub mod scatter;
pub(crate) mod util;

pub use am_router::AmRouter;
pub use bootstrap::{BootstrapConfig, bootstrap_communicator};
pub use communicator::UcxCommunicator;
pub use adaptive::{AdaptiveCommunicator, AdaptivePolicy, AllreduceAlgo};
pub use broadcast::BroadcastFuture as UcxBroadcastFuture;
pub use bruck::AllgatherFuture as UcxAllgatherFuture;
pub use pipelined_ring::{PipelineConfig, PipelinedRingAllreduceFuture};
pub use recursive_doubling::RecursiveDoublingFuture;
pub use scatter::ScatterFuture as UcxScatterFuture;
