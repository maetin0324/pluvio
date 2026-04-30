//! `#[ignore]`d integration test: launch with mpiexec.
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective --test mpi_allreduce_2proc \
//!   --release -- --ignored --nocapture
//! ```

#![cfg(feature = "mpi")]

use std::os::raw::c_int;

use pluvio_collective::mpi_backend::{disable_async_progress, MpiCommunicator, MpiReactor};
use pluvio_collective::{Collective, Communicator, Sum};
use pluvio_runtime::executor::Runtime;

fn run_allreduce_test() {
    disable_async_progress();
    let mut provided: c_int = 0;
    // SAFETY: argc/argv passed as null is allowed by MPI 3.1 §8.7.
    let rc = unsafe {
        mpi_sys::MPI_Init_thread(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            mpi_sys::RSMPI_THREAD_FUNNELED,
            &mut provided,
        )
    };
    assert_eq!(rc, mpi_sys::MPI_SUCCESS as i32, "MPI_Init_thread");

    let runtime = Runtime::new(1024);
    let reactor = MpiReactor::new();
    runtime.register_reactor("mpi", reactor.clone());

    let comm = MpiCommunicator::world(reactor).expect("MpiCommunicator::world");
    let rank = comm.rank();
    let size = comm.size();
    let len = 1024;
    let mut buf = vec![rank as f32 + 1.0; len];

    runtime
        .clone()
        .run_with_name_and_runtime("mpi_allreduce_test", async move {
            comm.allreduce::<Sum>(&mut buf).await.expect("allreduce");
            // Expected: 1 + 2 + ... + size = size*(size+1)/2
            let expected = (size * (size + 1) / 2) as f32;
            for (i, v) in buf.iter().enumerate() {
                assert!(
                    (*v - expected).abs() < 1e-3,
                    "rank {}: idx {} got {}, expected {}",
                    rank, i, v, expected,
                );
            }
            println!(
                "[mpi_allreduce_2proc] rank {}/{}: PASS (buf[0]={})",
                rank, size, buf[0],
            );
        });

    // SAFETY: matched call to MPI_Init_thread.
    unsafe {
        mpi_sys::MPI_Finalize();
    }
}

#[test]
#[ignore]
fn mpi_allreduce_2proc() {
    run_allreduce_test();
}
