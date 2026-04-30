//! Phase 1 demo: MPI `Iallreduce` and `Iscatter` driven by Pluvio's runtime.
//!
//! Run with:
//! ```sh
//! mpiexec -n 2 cargo run --example coll_mpi_example --release
//! ```

use std::os::raw::c_int;

use pluvio_collective::mpi_backend::{disable_async_progress, MpiCommunicator, MpiReactor};
use pluvio_collective::{Collective, Communicator, Sum};
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
    const PER_RANK: usize = 1024;
    const ROOT: usize = 0;

    // Allreduce input: each rank contributes a constant vector of `rank+1`.
    let mut allreduce_buf = vec![rank as f32 + 1.0; PER_RANK];

    // Scatter input: at root, fill chunk `r` with the value `(r as f32) + 0.5`
    // so each rank can verify it received its own chunk.
    let scatter_send: Option<Vec<f32>> = if rank == ROOT {
        Some(
            (0..size)
                .flat_map(|r| std::iter::repeat_n(r as f32 + 0.5, PER_RANK))
                .collect(),
        )
    } else {
        None
    };
    let mut scatter_recv = vec![0.0_f32; PER_RANK];

    runtime.clone().run_with_name_and_runtime("coll_mpi_example", async move {
        // --- Allreduce ---
        comm.allreduce::<Sum>(&mut allreduce_buf).await.unwrap();
        // Expected sum = 1 + 2 + ... + size = size * (size + 1) / 2
        let expected = (size * (size + 1) / 2) as f32;
        let max_abs_err = allreduce_buf
            .iter()
            .map(|v| (v - expected).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "[allreduce] rank {}/{}: buf[0]={}, expected={}, max_abs_err={}",
            rank, size, allreduce_buf[0], expected, max_abs_err,
        );

        // --- Scatter ---
        comm.scatter(scatter_send.as_deref(), &mut scatter_recv, ROOT)
            .await
            .unwrap();
        let expected_chunk = rank as f32 + 0.5;
        let scatter_err = scatter_recv
            .iter()
            .map(|v| (v - expected_chunk).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "[scatter ] rank {}/{}: recv[0]={}, expected={}, max_abs_err={}",
            rank, size, scatter_recv[0], expected_chunk, scatter_err,
        );
    });

    // SAFETY: matched call to MPI_Init_thread above.
    unsafe {
        mpi_sys::MPI_Finalize();
    }
}
