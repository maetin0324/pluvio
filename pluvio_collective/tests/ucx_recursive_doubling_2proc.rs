//! `#[ignore]`d integration test: recursive-doubling allreduce.
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective \
//!   --test ucx_recursive_doubling_2proc -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(feature = "ucx")]

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{
    AmRouter, BootstrapConfig, UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::{BitXor, Communicator, Sum};
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{Context, UCXReactor};

fn build_comm(
    bootstrap: &BootstrapConfig,
    runtime: &std::rc::Rc<Runtime>,
) -> (
    Rc<UcxCommunicator>,
    pluvio_ucx::worker::am::AmStream,
) {
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor);

    let context = Context::new().expect("Context::new");
    let worker = context.create_worker().expect("create_worker");
    let am_stream = worker.am_stream(COLLECTIVE_AM_ID).expect("am_stream");
    let am_stream_for_close = am_stream.clone();
    let router = AmRouter::new();

    let endpoints = bootstrap_communicator(&worker, bootstrap).expect("bootstrap");
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

    (comm, am_stream_for_close)
}

#[test]
#[ignore]
fn ucx_rd_f32_sum_2proc() {
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let (comm, am_close) = build_comm(&bootstrap, &runtime);

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("ucx_rd_f32", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let mut buf = vec![rank as f32 + 1.0; 1024];

            comm_clone
                .allreduce_recursive_doubling::<f32, Sum>(&mut buf)
                .await
                .expect("allreduce_recursive_doubling");

            let expected = (size * (size + 1) / 2) as f32;
            for v in buf.iter() {
                assert!(
                    (*v - expected).abs() < 1e-3,
                    "rank {}: got {}, expected {}",
                    rank, v, expected,
                );
            }
            println!("[ucx_rd_f32_sum] rank {}/{}: PASS", rank, size);
            am_close.close();
        });
}

#[test]
#[ignore]
fn ucx_rd_matches_ring_bitexact_u32_xor() {
    // Recursive doubling and ring traverse the same set of inputs but in a
    // different reduction order. For an associative + commutative op like
    // u32 XOR, both must produce identical results.
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let (comm, am_close) = build_comm(&bootstrap, &runtime);

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("ucx_rd_xor_cross", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let buf_len = size * 256;
            let initial: Vec<u32> = (0..buf_len)
                .map(|i| ((rank as u32) << 16) ^ (i as u32).wrapping_mul(2_654_435_761))
                .collect();

            let mut a = initial.clone();
            comm_clone
                .allreduce_recursive_doubling::<u32, BitXor>(&mut a)
                .await
                .expect("rd");

            let mut b = initial.clone();
            comm_clone
                .allreduce_with::<u32, BitXor>(&mut b)
                .await
                .expect("ring");

            assert_eq!(a, b, "rank {}: rd != ring", rank);
            println!("[ucx_rd_xor_cross] rank {}/{}: PASS", rank, size);
            am_close.close();
        });
}
