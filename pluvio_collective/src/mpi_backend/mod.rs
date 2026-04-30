//! MPI backend (Phase 1).
//!
//! Implements `Allreduce` by wrapping `MPI_Iallreduce` and driving completion
//! through a dedicated `MpiReactor` registered on the Pluvio runtime.

pub mod allreduce;
pub mod communicator;
pub mod datatype;
pub mod reactor;
pub mod scatter;

pub use allreduce::AllreduceFuture;
pub use communicator::MpiCommunicator;
pub use datatype::MpiDatatype;
pub use reactor::MpiReactor;
pub use scatter::ScatterFuture as MpiScatterFuture;

/// Apply environment variables that disable MPI internal async progress
/// threads, so progress is driven solely by `MpiReactor::poll`.
///
/// Call this **before** `MPI_Init` (or `MPI_Init_thread`).
pub fn disable_async_progress() {
    // MPICH variants
    unsafe {
        std::env::set_var("MPICH_ASYNC_PROGRESS", "0");
        std::env::set_var("MPIR_CVAR_ASYNC_PROGRESS", "0");
        // OpenMPI: prevent the UCX PML from blocking-spinning on its own thread.
        std::env::set_var("OMPI_MCA_pml_ucx_progress_iterations", "0");
    }
}
