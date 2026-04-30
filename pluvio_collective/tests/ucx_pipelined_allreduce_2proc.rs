//! `#[ignore]`d integration test: pipelined ring allreduce on top of UCX AM.
//!
//! ```sh
//! mpiexec -n 2 cargo test -p pluvio_collective \
//!   --test ucx_pipelined_allreduce_2proc -- --ignored --nocapture
//! ```

#![cfg(feature = "ucx")]

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::pipelined_ring::PipelineConfig;
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
fn ucx_pipelined_allreduce_micro_chunks_4_f32_sum() {
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let (comm, am_close) = build_comm(&bootstrap, &runtime);

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("ucx_pipelined_allreduce", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            // chunk_size = buf.len()/size = 4 * 256 = 1024 elements
            // micro_size = 1024 / 4 = 256 elements per micro-chunk
            let per_micro = 256;
            let micro_chunks = 4;
            let buf_len = size * per_micro * micro_chunks;
            let mut buf = vec![rank as f32 + 1.0; buf_len];

            let cfg = PipelineConfig {
                micro_chunks,
                max_in_flight: 8,
                proto: Some(pluvio_ucx::async_ucx::ucp::AmProto::Eager),
            };
            comm_clone
                .allreduce_pipelined::<f32, Sum>(&mut buf, cfg)
                .await
                .expect("allreduce_pipelined");

            let expected = (size * (size + 1) / 2) as f32;
            for (i, v) in buf.iter().enumerate() {
                assert!(
                    (*v - expected).abs() < 1e-3,
                    "rank {}: idx {} got {}, expected {}",
                    rank, i, v, expected,
                );
            }
            println!(
                "[ucx_pipelined_allreduce] rank {}/{}: PASS (M={}, max_in_flight={}, buf_len={})",
                rank, size, micro_chunks, cfg.max_in_flight, buf_len,
            );
            am_close.close();
        });
}

#[test]
#[ignore]
fn ucx_pipelined_allreduce_bitexact_u32_xor() {
    // u32 XOR is associative, so plain ring vs pipelined should be bit-exact.
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let (comm, am_close) = build_comm(&bootstrap, &runtime);

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("ucx_pipelined_xor", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let per_micro = 128;
            let micro_chunks = 8;
            let buf_len = size * per_micro * micro_chunks;
            // Each rank contributes a deterministic vector keyed by rank.
            let mut buf: Vec<u32> = (0..buf_len)
                .map(|i| ((rank as u32) << 16) ^ (i as u32).wrapping_mul(2_654_435_761))
                .collect();
            let initial = buf.clone();

            let cfg = PipelineConfig {
                micro_chunks,
                max_in_flight: 16,
                proto: Some(pluvio_ucx::async_ucx::ucp::AmProto::Eager),
            };
            comm_clone
                .allreduce_pipelined::<u32, BitXor>(&mut buf, cfg)
                .await
                .expect("allreduce_pipelined");

            // Re-compute the ground-truth xor from the deterministic input
            // using the same construction every rank uses, then xor across
            // ranks.
            let mut ground = vec![0u32; buf_len];
            for r in 0..size {
                for (i, g) in ground.iter_mut().enumerate() {
                    let v = ((r as u32) << 16) ^ (i as u32).wrapping_mul(2_654_435_761);
                    *g ^= v;
                }
            }
            assert_eq!(buf, ground, "rank {} produced wrong allreduce", rank);
            // Sanity: we mutated `initial` (otherwise the test is a no-op).
            assert_ne!(buf, initial, "rank {}: buf unchanged after allreduce", rank);
            println!(
                "[ucx_pipelined_allreduce_xor] rank {}/{}: PASS bit-exact (M={})",
                rank, size, micro_chunks,
            );
            am_close.close();
        });
}

#[test]
#[ignore]
fn ucx_pipelined_matches_plain_ring_micro_chunks_1() {
    // micro_chunks = 1 should behave identically to plain ring for the same
    // input, since there is no actual pipelining. We use bit-exact u32 XOR.
    let bootstrap = BootstrapConfig::from_env().expect("BootstrapConfig::from_env");
    let runtime = Runtime::new(1024);
    let (comm, am_close) = build_comm(&bootstrap, &runtime);

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("ucx_pipelined_m1", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            let buf_len = size * 256;
            let initial: Vec<u32> = (0..buf_len)
                .map(|i| ((rank as u32) * 1000 + i as u32) ^ 0xdead_beef)
                .collect();

            let mut a = initial.clone();
            comm_clone
                .allreduce_pipelined::<u32, BitXor>(
                    &mut a,
                    PipelineConfig {
                        micro_chunks: 1,
                        max_in_flight: 1,
                        proto: Some(pluvio_ucx::async_ucx::ucp::AmProto::Eager),
                    },
                )
                .await
                .expect("pipelined m=1");

            let mut b = initial.clone();
            // Plain ring path for the same input.
            comm_clone
                .allreduce_with::<u32, BitXor>(&mut b)
                .await
                .expect("plain ring");

            assert_eq!(a, b, "rank {}: pipelined(M=1) != plain ring", rank);
            println!(
                "[ucx_pipelined_allreduce] rank {}/{}: PASS plain==M1 ({} elems)",
                rank, size, buf_len,
            );
            am_close.close();
        });
}
