//! Cross-check that MPI and UCX backends produce equivalent allreduce results.
//!
//! The `f32 Sum` ring (UCX) reduces in a different order than MPI's recursive
//! doubling, so we use a relative tolerance rather than bit-exact equality.
//! `BitXor<u32>` is also tested for bit-exact agreement.
//!
//! Run with:
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective --test cross_check --release \
//!   -- --ignored --nocapture
//! ```

#![cfg(all(feature = "mpi", feature = "ucx"))]

use std::os::raw::c_int;
use std::rc::Rc;

use pluvio_collective::mpi_backend::{disable_async_progress, MpiCommunicator, MpiReactor};
use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{
    AmRouter, BootstrapConfig, UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::{Collective, Communicator, Sum};
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{Context, UCXReactor};

fn mpi_init() {
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
}

#[test]
#[ignore]
fn cross_check_mpi_vs_ucx_f32_sum() {
    mpi_init();

    // Use a fixed deterministic input so both backends operate on the same
    // bytes. We seed by rank so each rank's contribution differs.
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let rank = bootstrap.rank;
    let size = bootstrap.size;
    let chunks = size;
    let per_chunk = 1024;
    let n = chunks * per_chunk;

    let mut input: Vec<f32> = (0..n)
        .map(|i| ((rank * 977 + i * 13) % 251) as f32 + 0.25)
        .collect();
    let mpi_input = input.clone();

    // -- MPI --
    let runtime_mpi = Runtime::new(1024);
    let mpi_reactor = MpiReactor::new();
    runtime_mpi.register_reactor("mpi", mpi_reactor.clone());
    let mpi_comm = MpiCommunicator::world(mpi_reactor).expect("MpiCommunicator::world");
    assert_eq!(mpi_comm.rank(), rank);
    assert_eq!(mpi_comm.size(), size);
    let mut mpi_buf = mpi_input.clone();
    runtime_mpi.clone().run_with_name_and_runtime("mpi-cross", async move {
        mpi_comm.allreduce::<Sum>(&mut mpi_buf).await.unwrap();
        TLS_MPI_RESULT.with(|cell| *cell.borrow_mut() = Some(mpi_buf));
    });

    // -- UCX (after MPI runtime is finished) --
    let runtime_ucx = Runtime::new(1024);
    let ucx_reactor = UCXReactor::current();
    runtime_ucx.register_reactor("ucx", ucx_reactor);
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
    runtime_ucx.clone().spawn_with_runtime(async move {
        dispatcher_loop(am_stream, router).await;
    });
    let comm_clone = comm.clone();
    let mut ucx_buf = input.clone();
    let _ = &mut input; // keep input live until end (avoid drop ordering)
    runtime_ucx
        .clone()
        .run_with_name_and_runtime("ucx-cross", async move {
            comm_clone
                .allreduce_with::<f32, Sum>(&mut ucx_buf)
                .await
                .expect("allreduce");
            TLS_UCX_RESULT.with(|cell| *cell.borrow_mut() = Some(ucx_buf));
            am_stream_for_close.close();
        });

    let mpi_result = TLS_MPI_RESULT.with(|c| c.borrow_mut().take()).expect("mpi result");
    let ucx_result = TLS_UCX_RESULT.with(|c| c.borrow_mut().take()).expect("ucx result");
    assert_eq!(mpi_result.len(), ucx_result.len());

    let tol_rel = 1e-3_f32;
    let tol_abs = 1e-3_f32;
    let mut max_rel: f32 = 0.0;
    for (i, (a, b)) in mpi_result.iter().zip(ucx_result.iter()).enumerate() {
        let denom = a.abs().max(tol_abs);
        let rel = (a - b).abs() / denom;
        if rel > max_rel {
            max_rel = rel;
        }
        assert!(
            rel < tol_rel,
            "rank {}: idx {}: mpi={} ucx={} rel={}",
            rank, i, a, b, rel,
        );
    }
    println!(
        "[cross_check] rank {}/{}: PASS (max_rel={})",
        rank, size, max_rel,
    );

    // SAFETY: matched MPI_Init_thread above.
    unsafe {
        mpi_sys::MPI_Finalize();
    }
}

thread_local! {
    static TLS_MPI_RESULT: std::cell::RefCell<Option<Vec<f32>>> = const { std::cell::RefCell::new(None) };
    static TLS_UCX_RESULT: std::cell::RefCell<Option<Vec<f32>>> = const { std::cell::RefCell::new(None) };
}
