// MPI + UCX integration test to verify compatibility issues
// This example tests if MPI and UCX can coexist without context conflicts

use pluvio_runtime::executor::Runtime;
use pluvio_ucx::UCXReactor;
use std::rc::Rc;
use mpi::traits::*;
use std::time::Duration;
use std::path::PathBuf;
use std::mem::MaybeUninit;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() {
    // Initialize MPI first (like benchfsd_mpi does)
    let universe = mpi::initialize().unwrap();
    let world = universe.world();
    let mpi_rank = world.rank();
    let mpi_size = world.size();

    // Setup logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "Info".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    // Get registry directory from args or use default
    let args: Vec<String> = std::env::args().collect();
    let registry_dir = if args.len() > 1 {
        PathBuf::from(&args[1])
    } else {
        PathBuf::from("/tmp/mpi_ucx_test")
    };

    // Create registry directory (rank 0 only)
    if mpi_rank == 0 {
        std::fs::create_dir_all(&registry_dir).ok();
        println!("MPI + UCX Test");
        println!("Total MPI processes: {}", mpi_size);
        println!("Registry directory: {:?}", registry_dir);
    }

    // MPI barrier to ensure directory is created
    world.barrier();

    // Create pluvio runtime
    let runtime = Runtime::new(256);
    pluvio_runtime::set_runtime(runtime.clone());

    // Create UCX reactor and context
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor.clone());

    // Test UCX initialization with MPI
    let result = test_ucx_with_mpi(
        runtime.clone(),
        ucx_reactor.clone(),
        mpi_rank,
        mpi_size,
        &registry_dir,
    );

    match result {
        Ok(_) => {
            if mpi_rank == 0 {
                println!("✓ MPI + UCX test completed successfully!");
            }
        }
        Err(e) => {
            eprintln!("✗ Rank {}: MPI + UCX test failed: {}", mpi_rank, e);
        }
    }

    // Final MPI barrier before finalization
    world.barrier();

    if mpi_rank == 0 {
        println!("Test complete. All ranks finished.");
    }
}

fn test_ucx_with_mpi(
    _runtime: Rc<Runtime>,
    reactor: Rc<UCXReactor>,
    mpi_rank: i32,
    mpi_size: i32,
    registry_dir: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create UCX context and worker (this is where conflicts may occur)
    let ucx_context = Rc::new(pluvio_ucx::Context::new()?);
    let worker = ucx_context.create_worker()?;
    reactor.register_worker(worker.clone());

    println!("Rank {}: UCX worker created successfully", mpi_rank);

    // Clone registry_dir to move into async block
    let registry_dir = registry_dir.clone();

    // Test Socket connection mode
    pluvio_runtime::run_with_name("mpi_ucx_test", async move {
        // Rank 0 acts as server, others as clients
        if mpi_rank == 0 {
            // Server: Create listener and accept connections
            println!("Rank 0: Starting as server");

            // Create listener on Socket address
            let bind_addr = "0.0.0.0:20000".parse().unwrap();
            let mut listener = worker.create_listener(bind_addr)?;
            println!("Rank 0: Listener created on {:?}", listener.socket_addr()?);

            // Also get and save WorkerAddress for testing
            let worker_addr = worker.address()?;
            let addr_file = registry_dir.join(format!("node_{}.addr", mpi_rank));
            std::fs::write(&addr_file, worker_addr.as_ref())?;
            println!("Rank 0: WorkerAddress saved ({} bytes)", worker_addr.as_ref().len());

            // Accept connections from other ranks
            for i in 1..mpi_size {
                println!("Rank 0: Waiting for connection from rank {}", i);
                let conn_request = listener.next().await;
                let endpoint = worker.accept(conn_request).await?;
                println!("Rank 0: Accepted connection from rank {}", i);

                // Test data exchange
                let test_data = b"Hello from rank 0";
                endpoint.stream_send(test_data).await?;

                let mut recv_buf: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); 32];
                let received = endpoint.stream_recv(&mut recv_buf).await?;
                println!("Rank 0: Received {} bytes from rank {}", received, i);
            }

        } else {
            // Client: Connect to rank 0
            println!("Rank {}: Starting as client", mpi_rank);

            // Wait a bit for server to be ready
            futures_timer::Delay::new(Duration::from_millis(500 * mpi_rank as u64)).await;

            // Get server's WorkerAddress (for testing WorkerAddress mode)
            let server_addr_file = registry_dir.join("node_0.addr");

            // Wait for server to write address
            let mut retries = 0;
            while !server_addr_file.exists() && retries < 30 {
                futures_timer::Delay::new(Duration::from_millis(100)).await;
                retries += 1;
            }

            if !server_addr_file.exists() {
                return Err(format!("Rank {}: Server address file not found", mpi_rank).into());
            }

            // Test Socket connection
            println!("Rank {}: Testing Socket connection mode", mpi_rank);
            let server_socket = "127.0.0.1:20000".parse().unwrap();
            let endpoint = worker.connect_socket(server_socket).await?;
            println!("Rank {}: Socket connection established", mpi_rank);

            // Test data exchange
            let mut recv_buf: Vec<MaybeUninit<u8>> = vec![MaybeUninit::uninit(); 32];
            let received = endpoint.stream_recv(&mut recv_buf).await?;
            println!("Rank {}: Received {} bytes", mpi_rank, received);

            let response = format!("Hello from rank {}", mpi_rank);
            endpoint.stream_send(response.as_bytes()).await?;
            println!("Rank {}: Sent response", mpi_rank);

            // Also test WorkerAddress exchange for InfiniBand lane setup
            println!("Rank {}: Getting WorkerAddress for InfiniBand", mpi_rank);
            let our_address = worker.address()?;
            println!("Rank {}: Our WorkerAddress is {} bytes", mpi_rank, our_address.as_ref().len());
        }

        println!("Rank {}: Test completed", mpi_rank);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });

    Ok(())
}