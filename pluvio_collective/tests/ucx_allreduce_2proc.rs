//! `#[ignore]`d integration test: launch with mpiexec (used only for rank/size
//! discovery; the actual collective uses the UCX backend).
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective --test ucx_allreduce_2proc \
//!   --release -- --ignored --nocapture
//! ```

#![cfg(feature = "ucx")]

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{
    AmRouter, BootstrapConfig, UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::{Communicator, Sum};
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{Context, UCXReactor};

#[test]
#[ignore]
fn ucx_allreduce_2proc() {
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
        .run_with_name_and_runtime("ucx_allreduce_2proc", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let chunks = size;
            let per_chunk = 256;
            let mut buf = vec![rank as f32 + 1.0; chunks * per_chunk];

            comm_clone
                .allreduce_with::<f32, Sum>(&mut buf)
                .await
                .expect("allreduce");

            let expected = (size * (size + 1) / 2) as f32;
            for (i, v) in buf.iter().enumerate() {
                assert!(
                    (*v - expected).abs() < 1e-3,
                    "rank {}: idx {} got {}, expected {}",
                    rank, i, v, expected,
                );
            }
            println!(
                "[ucx_allreduce_2proc] rank {}/{}: PASS (buf[0]={})",
                rank, size, buf[0],
            );
            am_stream_for_close.close();
        });
}
