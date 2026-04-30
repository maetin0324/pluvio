//! Phase 3 demo: pipelined ring allreduce on UCX Active Messages.
//!
//! Run with:
//! ```sh
//! mpiexec -n 2 \
//!   -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 -x PLUVIO_COLL_ROOT_PORT=14500 \
//!   cargo run --example coll_ucx_pipelined --release
//! ```

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{
    AmRouter, BootstrapConfig, PipelineConfig, UcxCommunicator, bootstrap_communicator,
};
use pluvio_collective::{Communicator, Sum};
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
    tracing::info!(
        "rank={} size={} root={}:{}",
        bootstrap.rank,
        bootstrap.size,
        bootstrap.root_host,
        bootstrap.root_port,
    );

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
    runtime.clone().run_with_name_and_runtime("coll_ucx_pipelined", async move {
        let rank = comm_clone.rank();
        let size = comm_clone.size();
        let micro_chunks = 4;
        let per_micro = 1024;
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
        let max_abs_err = buf
            .iter()
            .map(|v| (v - expected).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "[pipelined] rank {}/{}: buf_len={}, M={}, max_in_flight={}, buf[0]={}, expected={}, max_abs_err={}",
            rank, size, buf_len, micro_chunks, cfg.max_in_flight,
            buf[0], expected, max_abs_err,
        );
        am_close.close();
    });

    Ok(())
}
