//! Phase 3 demo: `AdaptiveCommunicator` picks an allreduce algorithm based
//! on message size.
//!
//! Run with:
//! ```sh
//! mpiexec -n 2 \
//!   -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 -x PLUVIO_COLL_ROOT_PORT=14501 \
//!   cargo run --example coll_adaptive --release
//! ```

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{
    AdaptiveCommunicator, AdaptivePolicy, AllreduceAlgo, AmRouter, BootstrapConfig,
    UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::{Collective, Communicator, Sum};
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{Context, UCXReactor};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let bootstrap = BootstrapConfig::from_env()
        .map_err(|e| anyhow::anyhow!("bootstrap config: {}", e))?;

    let runtime = Runtime::new(1024);
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor);

    let context = Context::new()?;
    let worker = context.create_worker()?;
    let am_stream = worker.am_stream(COLLECTIVE_AM_ID)?;
    let am_close = am_stream.clone();
    let router = AmRouter::new();
    let endpoints = bootstrap_communicator(&worker, &bootstrap)
        .map_err(|e| anyhow::anyhow!("bootstrap: {}", e))?;

    let inner = Rc::new(UcxCommunicator::new(
        bootstrap.rank,
        bootstrap.size,
        worker.clone(),
        endpoints,
        router.clone(),
    ));
    let comm = Rc::new(AdaptiveCommunicator::new(inner, AdaptivePolicy::default()));

    runtime.clone().spawn_with_runtime(async move {
        dispatcher_loop(am_stream, router).await;
    });

    let comm_clone = comm.clone();
    runtime
        .clone()
        .run_with_name_and_runtime("coll_adaptive", async move {
            let rank = comm_clone.rank();
            let size = comm_clone.size();
            // Sweep across a few message sizes to demonstrate the cutovers.
            let sizes_elements: &[usize] = &[64, 1024, 16 * 1024, 1024 * 1024];
            for &n in sizes_elements {
                let n_aligned = n.div_ceil(size) * size; // multiple of size for ring
                let mut buf = vec![rank as f32 + 1.0; n_aligned];
                let chosen = comm_clone.chosen_algo::<f32>(n_aligned);
                comm_clone
                    .allreduce::<Sum>(&mut buf)
                    .await
                    .expect("allreduce");
                let expected = (size * (size + 1) / 2) as f32;
                let err = buf
                    .iter()
                    .map(|v| (v - expected).abs())
                    .fold(0.0_f32, f32::max);
                let label = match chosen {
                    AllreduceAlgo::PlainRing => "plain-ring",
                    AllreduceAlgo::PipelinedRing { .. } => "pipelined-ring",
                    AllreduceAlgo::RecursiveDoubling => "recursive-doubling",
                };
                println!(
                    "[adaptive] rank {}/{}: n_elems={:7}, bytes={:8}, algo={:<18} err={}",
                    rank,
                    size,
                    n_aligned,
                    n_aligned * 4,
                    label,
                    err,
                );
            }
            am_close.close();
        });

    Ok(())
}
