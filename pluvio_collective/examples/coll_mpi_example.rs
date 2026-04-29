//! Phase 1 demo: MPI Iallreduce driven by Pluvio's runtime.
//!
//! Run with:
//! ```sh
//! mpiexec -n 2 cargo run --example coll_mpi_example --release
//! ```

use std::os::raw::c_int;

use pluvio_collective::mpi_backend::{disable_async_progress, MpiCommunicator, MpiReactor};
use pluvio_collective::{Communicator, Sum};
use pluvio_runtime::executor::Runtime;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    disable_async_progress();
    let mut provided: c_int = 0;
    // SAFETY: argc/argv passed as null is allowed by MPI_Init_thread per
    // MPI 3.1 §8.7. THREAD_FUNNELED is sufficient because all MPI calls are
    // made from the main thread.
    let rc = unsafe {
        mpi_sys::MPI_Init_thread(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            mpi_sys::RSMPI_THREAD_FUNNELED,
            &mut provided,
        )
    };
    if rc != mpi_sys::MPI_SUCCESS as i32 {
        panic!("MPI_Init_thread failed with rc={}", rc);
    }

    let runtime = Runtime::new(1024);
    let reactor = MpiReactor::new();
    runtime.register_reactor("mpi", reactor.clone());

    let comm = MpiCommunicator::world(reactor).expect("MpiCommunicator::world failed");
    let rank = comm.rank();
    let size = comm.size();
    let mut buf = vec![rank as f32 + 1.0; 1024];

    runtime.clone().run_with_name_and_runtime("coll_mpi_example", async move {
        use pluvio_collective::Collective;
        comm.allreduce::<Sum>(&mut buf).await.unwrap();
        // Expected sum = 1 + 2 + ... + size = size * (size + 1) / 2
        let expected = (size * (size + 1) / 2) as f32;
        let max_abs_err = buf
            .iter()
            .map(|v| (v - expected).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "rank {}/{}: buf[0]={}, expected={}, max_abs_err={}",
            rank, size, buf[0], expected, max_abs_err,
        );
    });

    // SAFETY: matched call to MPI_Init_thread above.
    unsafe {
        mpi_sys::MPI_Finalize();
    }
}
