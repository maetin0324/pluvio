//! UCX backend (Phase 2): ring allreduce on top of pluvio_ucx Active Messages.

pub mod am_router;
pub mod bootstrap;
pub mod communicator;
pub mod ring;
pub mod scatter;

pub use am_router::AmRouter;
pub use bootstrap::{BootstrapConfig, bootstrap_communicator};
pub use communicator::UcxCommunicator;
pub use scatter::ScatterFuture as UcxScatterFuture;
