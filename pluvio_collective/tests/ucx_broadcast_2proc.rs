//! `#[ignore]`d integration test: binomial-tree broadcast.
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective \
//!   --test ucx_broadcast_2proc -- --ignored --nocapture --test-threads=1
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
fn ucx_broadcast_2proc() {
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
        .run_with_name_and_runtime("ucx_broadcast", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let root = 0usize;
            let n = 1024;
            let mut buf: Vec<u32> = if rank == root {
                (0..n).map(|i| 42 ^ (i as u32 * 17)).collect()
            } else {
                vec![0u32; n]
            };

            comm_clone.broadcast_with(&mut buf, root).await.expect("broadcast");

            for (i, v) in buf.iter().enumerate() {
                let expected = 42_u32 ^ (i as u32 * 17);
                assert_eq!(
                    *v, expected,
                    "rank {} idx {}: got {}, expected {}",
                    rank, i, v, expected,
                );
            }
            println!("[ucx_broadcast] rank {}/{}: PASS", rank, size);
            am_close.close();
        });
}
