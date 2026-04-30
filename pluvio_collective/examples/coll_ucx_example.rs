//! Phase 2 demo: ring allreduce and direct-send scatter on top of UCX Active
//! Messages.
//!
//! Run with mpiexec (which sets `OMPI_COMM_WORLD_RANK/SIZE`), or by manually
//! exporting `PLUVIO_COLL_RANK`, `PLUVIO_COLL_SIZE`, `PLUVIO_COLL_ROOT_HOST`,
//! `PLUVIO_COLL_ROOT_PORT` per process:
//!
//! ```sh
//! mpiexec -n 2 cargo run --example coll_ucx_example --release
//! ```

use std::rc::Rc;

use pluvio_collective::ucx_backend::am_router::dispatcher_loop;
use pluvio_collective::ucx_backend::communicator::COLLECTIVE_AM_ID;
use pluvio_collective::ucx_backend::{AmRouter, BootstrapConfig, UcxCommunicator, bootstrap_communicator};
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
        "bootstrap: rank={} size={} root={}:{}",
        bootstrap.rank,
        bootstrap.size,
        bootstrap.root_host,
        bootstrap.root_port,
    );

    let runtime = Runtime::new(1024);
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor.clone());

    let context = Context::new()?;
    let worker = context.create_worker()?;

    // AM stream registration: must happen before bootstrap so messages from
    // peers don't get dropped.
    let am_stream = worker.am_stream(COLLECTIVE_AM_ID)?;
    let am_stream_for_close = am_stream.clone();
    let router = AmRouter::new();

    let endpoints = bootstrap_communicator(&worker, &bootstrap)
        .map_err(|e| anyhow::anyhow!("bootstrap_communicator: {}", e))?;

    let comm = Rc::new(UcxCommunicator::new(
        bootstrap.rank,
        bootstrap.size,
        worker.clone(),
        endpoints,
        router.clone(),
    ));

    // Spawn the AM dispatcher.
    runtime.clone().spawn_with_runtime(async move {
        dispatcher_loop(am_stream, router).await;
    });

    let comm_clone = comm.clone();
    runtime.clone().run_with_name_and_runtime("coll_ucx_example", async move {
        let rank = comm_clone.rank();
        let size = comm_clone.size();
        let chunks = size; // ensure divisibility
        let per_chunk = 256;
        const ROOT: usize = 0;

        // --- Allreduce ---
        let mut buf = vec![rank as f32 + 1.0; chunks * per_chunk];
        comm_clone
            .allreduce_with::<f32, Sum>(&mut buf)
            .await
            .expect("allreduce");

        let expected = (size * (size + 1) / 2) as f32;
        let max_abs_err = buf
            .iter()
            .map(|v| (v - expected).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "[allreduce] rank {}/{}: buf[0]={}, expected={}, max_abs_err={}",
            rank, size, buf[0], expected, max_abs_err,
        );

        // --- Scatter ---
        let scatter_send: Option<Vec<f32>> = if rank == ROOT {
            Some(
                (0..size)
                    .flat_map(|r| std::iter::repeat(r as f32 + 0.5).take(per_chunk))
                    .collect(),
            )
        } else {
            None
        };
        let mut scatter_recv = vec![0.0_f32; per_chunk];
        comm_clone
            .scatter_with(scatter_send.as_deref(), &mut scatter_recv, ROOT)
            .await
            .expect("scatter");
        let expected_chunk = rank as f32 + 0.5;
        let scatter_err = scatter_recv
            .iter()
            .map(|v| (v - expected_chunk).abs())
            .fold(0.0_f32, f32::max);
        println!(
            "[scatter  ] rank {}/{}: recv[0]={}, expected={}, max_abs_err={}",
            rank, size, scatter_recv[0], expected_chunk, scatter_err,
        );

        am_stream_for_close.close();
    });

    Ok(())
}
