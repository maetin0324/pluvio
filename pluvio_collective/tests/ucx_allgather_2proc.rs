//! `#[ignore]`d integration test: Bruck allgather.
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective \
//!   --test ucx_allgather_2proc -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(feature = "ucx")]

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{
    AmRouter, BootstrapConfig, UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::Communicator;
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{Context, UCXReactor};

#[test]
#[ignore]
fn ucx_bruck_allgather_2proc() {
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor);

    let context = Context::new().expect("Context::new");
    let worker = context.create_worker().expect("create_worker");
    let am_stream = worker.am_stream(COLLECTIVE_AM_ID).expect("am_stream");
    let am_close = am_stream.clone();
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
        .run_with_name_and_runtime("ucx_allgather", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            const PER_RANK: usize = 256;
            let send_buf: Vec<u32> = (0..PER_RANK)
                .map(|i| (rank as u32) * 1_000 + i as u32)
                .collect();
            let mut recv_buf = vec![0u32; PER_RANK * size];

            comm_clone
                .allgather_with(&send_buf, &mut recv_buf)
                .await
                .expect("allgather");

            for r in 0..size {
                for i in 0..PER_RANK {
                    let expected = (r as u32) * 1_000 + i as u32;
                    assert_eq!(
                        recv_buf[r * PER_RANK + i],
                        expected,
                        "rank {} idx {} (chunk {}): got {}, expected {}",
                        rank, i, r, recv_buf[r * PER_RANK + i], expected,
                    );
                }
            }
            println!("[ucx_allgather] rank {}/{}: PASS", rank, size);
            am_close.close();
        });
}
