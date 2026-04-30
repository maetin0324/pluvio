//! `#[ignore]`d integration test for `Scatter` on both backends.
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective --test scatter_2proc \
//!   --release -- --ignored --nocapture
//! ```
//!
//! Each rank checks that after `scatter(root=0, recv_buf)`, its `recv_buf`
//! holds the chunk of `send_buf` indexed by its own rank.

#![cfg(any(feature = "mpi", feature = "ucx"))]

#[cfg(feature = "mpi")]
use std::os::raw::c_int;

#[cfg(feature = "mpi")]
use pluvio_collective::mpi_backend::{disable_async_progress, MpiCommunicator, MpiReactor};
#[cfg(feature = "ucx")]
use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
#[cfg(feature = "ucx")]
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
#[cfg(feature = "ucx")]
use pluvio_collective::ucx_backend::{
    AmRouter, BootstrapConfig, UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::{Collective, Communicator};
use pluvio_runtime::executor::Runtime;
#[cfg(feature = "ucx")]
use std::rc::Rc;

const PER_CHUNK: usize = 256;

/// Build the input buffer at root: chunk `r` is filled with the value
/// `r * 1000 + i` so we can detect off-by-one in offset/length easily.
fn root_input(size: usize) -> Vec<u32> {
    (0..size)
        .flat_map(|r| (0..PER_CHUNK).map(move |i| (r * 1000 + i) as u32))
        .collect()
}

fn expected_chunk(rank: usize) -> Vec<u32> {
    (0..PER_CHUNK).map(|i| (rank * 1000 + i) as u32).collect()
}

#[cfg(feature = "mpi")]
#[test]
#[ignore]
fn mpi_scatter_2proc() {
    disable_async_progress();
    let mut provided: c_int = 0;
    // SAFETY: argc/argv null per MPI 3.1 §8.7.
    let rc = unsafe {
        mpi_sys::MPI_Init_thread(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            mpi_sys::RSMPI_THREAD_FUNNELED,
            &mut provided,
        )
    };
    assert_eq!(rc, mpi_sys::MPI_SUCCESS as i32);

    let runtime = Runtime::new(1024);
    let reactor = MpiReactor::new();
    runtime.register_reactor("mpi", reactor.clone());
    let comm = MpiCommunicator::world(reactor).expect("MpiCommunicator::world");
    let rank = comm.rank();
    let size = comm.size();
    let root = 0usize;

    let send_buf: Option<Vec<u32>> = if rank == root { Some(root_input(size)) } else { None };
    let mut recv_buf = vec![0u32; PER_CHUNK];

    runtime
        .clone()
        .run_with_name_and_runtime("mpi_scatter", async move {
            comm.scatter(send_buf.as_deref(), &mut recv_buf, root)
                .await
                .expect("scatter");
            assert_eq!(recv_buf, expected_chunk(rank));
            println!("[mpi_scatter_2proc] rank {}/{}: PASS", rank, size);
        });

    // SAFETY: matched MPI_Init_thread above.
    unsafe {
        mpi_sys::MPI_Finalize();
    }
}

#[cfg(feature = "ucx")]
#[test]
#[ignore]
fn ucx_scatter_2proc() {
    use pluvio_ucx::{Context, UCXReactor};

    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor);

    let context = Context::new().expect("Context::new");
    let worker = context.create_worker().expect("create_worker");
    let am_stream = worker.am_stream(COLLECTIVE_AM_ID).expect("am_stream");
    let am_stream_for_close = am_stream.clone();
    let router = AmRouter::new();

    let endpoints = bootstrap_communicator(&worker, &bootstrap).expect("bootstrap");
    let comm = Rc::new(UcxCommunicator::new(
        bootstrap.rank,
        bootstrap.size,
        worker.clone(),
        endpoints,
        router.clone(),
    ));

    runtime.clone().spawn_with_runtime(async move {
        dispatcher_loop(am_stream, router).await;
    });

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("ucx_scatter", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let root = 0usize;
            let send_buf: Option<Vec<u32>> =
                if rank == root { Some(root_input(size)) } else { None };
            let mut recv_buf = vec![0u32; PER_CHUNK];
            comm_clone
                .scatter_with(send_buf.as_deref(), &mut recv_buf, root)
                .await
                .expect("scatter");
            assert_eq!(recv_buf, expected_chunk(rank));
            println!("[ucx_scatter_2proc] rank {}/{}: PASS", rank, size);
            am_stream_for_close.close();
        });
}
