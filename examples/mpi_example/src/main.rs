// MPI + UCX integration test to verify compatibility issues
// This example tests if MPI and UCX can coexist without context conflicts
// Server/Client ratio: N/2:N/2 (even ranks are servers, odd ranks are clients)
// Uses Active Messages and pluvio_uring for file I/O

use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{UCXReactor, async_ucx::ucp::AmMsg};
use pluvio_uring::file::DmaFile;
use pluvio_uring::reactor::{IoUringReactor, register_file};
use std::rc::Rc;
use mpi::traits::*;
use std::time::Duration;
use std::path::PathBuf;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// Test parameters
const DATA_SIZE: usize = 1 << 20; // 1 MiB
const NUM_TRANSFERS: usize = 128; // Total transfers per client
const TOTAL_SIZE: usize = DATA_SIZE * NUM_TRANSFERS; // 128 MiB
const WINDOW_SIZE: usize = 32; // Pipeline window size

// AM IDs
const REQUEST_AM_ID: u16 = 12;
const RESPONSE_AM_ID: u32 = 16;

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
                .unwrap_or_else(|_| "info".into()),
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
        println!("MPI + UCX Remote I/O Test (AM + DmaFile)");
        println!("Total MPI processes: {}", mpi_size);
        println!("Server ranks (even): {}", mpi_size / 2 + mpi_size % 2);
        println!("Client ranks (odd): {}", mpi_size - (mpi_size / 2 + mpi_size % 2));
        println!("Registry directory: {:?}", registry_dir);
        println!("Data size per transfer: {} MiB", DATA_SIZE / (1 << 20));
        println!("Transfers per client: {}", NUM_TRANSFERS);
        println!("Total size per client: {} MiB", TOTAL_SIZE / (1 << 20));
    }

    // MPI barrier to ensure directory is created
    world.barrier();

    // Create pluvio runtime
    let runtime = Runtime::new(1024);
    pluvio_runtime::set_runtime(runtime.clone());

    // Create io_uring reactor
    let uring_reactor = IoUringReactor::builder()
        .queue_size(2048)
        .buffer_size(DATA_SIZE)
        .submit_depth(64)
        .wait_submit_timeout(Duration::from_millis(1))
        .wait_complete_timeout(Duration::from_millis(1))
        .build();
    runtime.register_reactor("io_uring", uring_reactor.clone());

    // Create UCX reactor and context
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor.clone());

    // Test UCX initialization with MPI
    let result = test_ucx_with_mpi(
        runtime.clone(),
        uring_reactor.clone(),
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
    runtime: Rc<Runtime>,
    _uring_reactor: Rc<IoUringReactor>,
    reactor: Rc<UCXReactor>,
    mpi_rank: i32,
    _mpi_size: i32,
    registry_dir: &PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create UCX context and worker (this is where conflicts may occur)
    let ucx_context = Rc::new(pluvio_ucx::Context::new()?);
    let worker = ucx_context.create_worker()?;
    reactor.register_worker(worker.clone());

    println!("Rank {}: UCX worker created successfully", mpi_rank);

    // Clone registry_dir to move into async block
    let registry_dir = registry_dir.clone();

    // Determine role: even ranks are servers, odd ranks are clients
    let is_server = mpi_rank % 2 == 0;

    if is_server {
        println!("Rank {}: Role = SERVER", mpi_rank);
    } else {
        println!("Rank {}: Role = CLIENT", mpi_rank);
    }

    // Test Socket connection mode with AM and file I/O
    pluvio_runtime::run_with_name("mpi_ucx_test", async move {
        if is_server {
            // Server: Create listener and handle remote I/O requests
            run_server(runtime, worker, mpi_rank, registry_dir).await
        } else {
            // Client: Connect to server and send I/O requests
            run_client(worker, mpi_rank, registry_dir).await
        }
    });

    Ok(())
}

async fn run_server(
    _runtime: Rc<Runtime>,
    worker: Rc<pluvio_ucx::Worker>,
    mpi_rank: i32,
    registry_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Rank {}: Starting as server", mpi_rank);

    // Calculate deterministic port: 20000 + server_index
    let server_index = mpi_rank / 2;
    let port = 20000 + server_index as u16;
    let bind_addr = format!("0.0.0.0:{}", port).parse().unwrap();

    let mut listener = worker.create_listener(bind_addr)?;
    println!("Rank {}: Listener created on {:?}", mpi_rank, listener.socket_addr()?);

    // Get hostname for cross-node connections
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_else(|| "localhost".to_string())
        .trim()
        .to_string();

    // Save socket address (hostname:port) for client connection
    let socket_info = format!("{}:{}", hostname, port);
    let socket_file = registry_dir.join(format!("server_{}.socket", server_index));
    std::fs::write(&socket_file, &socket_info)?;
    println!("Rank {}: Socket info saved to {:?}: {}", mpi_rank, socket_file, socket_info);

    // Save WorkerAddress for debugging
    let worker_addr = worker.address()?;
    let addr_file = registry_dir.join(format!("server_{}.addr", server_index));
    std::fs::write(&addr_file, worker_addr.as_ref())?;
    println!("Rank {}: WorkerAddress saved to {:?} ({} bytes)", mpi_rank, addr_file, worker_addr.as_ref().len());

    // Create DMA file for I/O
    let target_file = registry_dir.join(format!("mpi_test_rank_{}.data", mpi_rank));
    let file = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&target_file)?;
    let fd = file.as_raw_fd();
    file.set_len(TOTAL_SIZE as u64)?;
    register_file(fd);

    let dma_file = Rc::new(DmaFile::new(file));
    dma_file.fallocate(TOTAL_SIZE as u64).await?;

    println!("Rank {}: DMA file created: {:?} ({} MiB)",
             mpi_rank, target_file, TOTAL_SIZE / (1 << 20));

    // Create AM stream for responses
    let stream = Rc::new(worker.am_stream(REQUEST_AM_ID)?);

    // Accept connection
    println!("Rank {}: Waiting for client connection", mpi_rank);
    let conn_request = listener.next().await;
    let endpoint = worker.accept(conn_request).await?;
    println!("Rank {}: Accepted client connection", mpi_rank);

    // Exchange WorkerAddress to enable InfiniBand lanes
    println!("Rank {}: Exchanging WorkerAddress for InfiniBand lane setup", mpi_rank);

    // Receive client's WorkerAddress length
    let mut len_buf: Vec<std::mem::MaybeUninit<u8>> = vec![std::mem::MaybeUninit::uninit(); 4];
    endpoint.stream_recv(&mut len_buf).await?;
    let len_buf: Vec<u8> = unsafe { std::mem::transmute(len_buf) };
    let client_addr_len = u32::from_le_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;

    // Receive client's WorkerAddress
    let mut client_addr_bytes: Vec<std::mem::MaybeUninit<u8>> = vec![std::mem::MaybeUninit::uninit(); client_addr_len];
    endpoint.stream_recv(&mut client_addr_bytes).await?;
    let _client_addr_bytes: Vec<u8> = unsafe { std::mem::transmute(client_addr_bytes) };

    // Send our WorkerAddress
    let our_address = worker.address()?;
    let our_addr_bytes: &[u8] = our_address.as_ref();
    let addr_len = our_addr_bytes.len() as u32;
    let addr_len_bytes = addr_len.to_le_bytes();
    endpoint.stream_send(&addr_len_bytes).await?;
    endpoint.stream_send(our_addr_bytes).await?;

    println!("Rank {}: WorkerAddress exchange complete (sent {} bytes, received {} bytes)",
             mpi_rank, our_addr_bytes.len(), client_addr_len);

    // Handle I/O requests with pipelining
    let mut jhs = futures::stream::FuturesUnordered::new();
    let mut inflight = 0usize;
    let mut total_received = 0usize;

    for i in 0..NUM_TRANSFERS {
        let msg = stream.wait_msg().await.ok_or("Stream closed")?;

        // Extract offset from header
        let header = msg.header();
        let offset = usize::from_le_bytes(header[0..8].try_into().unwrap());

        // Spawn write task
        let jh = pluvio_runtime::spawn_with_name(
            write_task(dma_file.clone(), offset, msg),
            format!("write_task_{}", i),
        );
        jhs.push(jh);
        inflight += 1;
        total_received += DATA_SIZE;

        // Drain completed tasks if window is full
        if inflight >= WINDOW_SIZE {
            use futures::StreamExt;
            let _ = jhs.next().await.ok_or("No tasks")??;
            inflight -= 1;
        }

        if (i + 1) % 32 == 0 {
            println!("Rank {}: Processed {}/{} transfers", mpi_rank, i + 1, NUM_TRANSFERS);
        }
    }

    // Wait for all remaining tasks
    use futures::StreamExt;
    while let Some(result) = jhs.next().await {
        result??;
    }

    println!("Rank {}: Server completed. Total received: {} MiB",
             mpi_rank, total_received / (1 << 20));

    Ok(())
}

async fn write_task(
    file: Rc<DmaFile>,
    offset: usize,
    mut msg: AmMsg,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Acquire buffer from DmaFile
    let mut buffer = file.acquire_buffer().await;
    let buf_ref = buffer.as_mut_slice();

    // Receive data into buffer
    msg.recv_data_single(buf_ref).await?;

    // Write to file
    file.write_fixed(buffer, offset as u64).await?;

    // Send reply
    unsafe {
        msg.reply(RESPONSE_AM_ID, &[], &[], false, None).await?;
    }

    Ok(())
}

async fn run_client(
    worker: Rc<pluvio_ucx::Worker>,
    mpi_rank: i32,
    registry_dir: PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Rank {}: Starting as client", mpi_rank);

    // Determine which server to connect to
    let server_index = (mpi_rank - 1) / 2;
    let server_rank = server_index * 2;

    println!("Rank {}: Connecting to server rank {}", mpi_rank, server_rank);

    // Wait for server socket file
    let server_socket_file = registry_dir.join(format!("server_{}.socket", server_index));
    let mut retries = 0;
    while !server_socket_file.exists() && retries < 60 {
        futures_timer::Delay::new(Duration::from_millis(100)).await;
        retries += 1;
    }

    if !server_socket_file.exists() {
        return Err(format!("Rank {}: Server socket file not found: {:?}",
                          mpi_rank, server_socket_file).into());
    }

    // Read server socket address
    let server_socket_str = std::fs::read_to_string(&server_socket_file)?;
    let server_socket: std::net::SocketAddr = server_socket_str.trim().parse()
        .map_err(|e| format!("Failed to parse server socket '{}': {}", server_socket_str, e))?;

    println!("Rank {}: Connecting to server at {}", mpi_rank, server_socket);
    let endpoint = worker.connect_socket(server_socket).await?;
    println!("Rank {}: Socket connection established to server rank {}",
             mpi_rank, server_rank);

    // Exchange WorkerAddress to enable InfiniBand lanes
    println!("Rank {}: Exchanging WorkerAddress for InfiniBand lane setup", mpi_rank);

    // Send our WorkerAddress
    let our_address = worker.address()?;
    let our_addr_bytes: &[u8] = our_address.as_ref();
    let addr_len = our_addr_bytes.len() as u32;
    let addr_len_bytes = addr_len.to_le_bytes();
    endpoint.stream_send(&addr_len_bytes).await?;
    endpoint.stream_send(our_addr_bytes).await?;

    // Receive server's WorkerAddress length
    let mut len_buf: Vec<std::mem::MaybeUninit<u8>> = vec![std::mem::MaybeUninit::uninit(); 4];
    endpoint.stream_recv(&mut len_buf).await?;
    let len_buf: Vec<u8> = unsafe { std::mem::transmute(len_buf) };
    let server_addr_len = u32::from_le_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;

    // Receive server's WorkerAddress
    let mut server_addr_bytes: Vec<std::mem::MaybeUninit<u8>> = vec![std::mem::MaybeUninit::uninit(); server_addr_len];
    endpoint.stream_recv(&mut server_addr_bytes).await?;
    let _server_addr_bytes: Vec<u8> = unsafe { std::mem::transmute(server_addr_bytes) };

    println!("Rank {}: WorkerAddress exchange complete (sent {} bytes, received {} bytes)",
             mpi_rank, our_addr_bytes.len(), server_addr_len);

    let endpoint = Rc::new(endpoint);

    // Create AM stream for responses
    let stream = Rc::new(worker.am_stream(16)?);

    // Prepare test data
    let test_data = vec![0xABu8; DATA_SIZE];
    let mut total_sent = 0usize;
    let start_time = std::time::Instant::now();

    // Send I/O requests with pipelining
    let mut inflight = 0usize;
    for i in 0..NUM_TRANSFERS {
        // Wait if window is full
        while inflight >= WINDOW_SIZE {
            let _ = stream.wait_msg().await.ok_or("Stream closed")?;
            inflight -= 1;
        }

        // Calculate offset
        let offset = i * DATA_SIZE;
        let offset_bytes = offset.to_le_bytes();

        // Send AM with offset in header and data
        endpoint
            .am_send(
                12,
                &offset_bytes,
                &test_data,
                true,
                Some(pluvio_ucx::async_ucx::ucp::AmProto::Rndv),
            )
            .await?;

        total_sent += DATA_SIZE;
        inflight += 1;

        if (i + 1) % 32 == 0 {
            println!("Rank {}: Sent {}/{} transfers", mpi_rank, i + 1, NUM_TRANSFERS);
        }
    }

    // Wait for remaining acknowledgments
    while inflight > 0 {
        let _ = stream.wait_msg().await.ok_or("Stream closed")?;
        inflight -= 1;
    }

    let elapsed = start_time.elapsed();
    println!("Rank {}: Client completed", mpi_rank);
    println!("Rank {}: Total sent: {} MiB", mpi_rank, total_sent / (1 << 20));
    println!("Rank {}: Elapsed time: {:.2} seconds", mpi_rank, elapsed.as_secs_f64());
    println!("Rank {}: Bandwidth: {:.2} MB/s",
             mpi_rank, total_sent as f64 / elapsed.as_secs_f64() / (1 << 20) as f64);
    println!("Rank {}: IOPS: {:.2} KIOPS",
             mpi_rank, NUM_TRANSFERS as f64 / elapsed.as_secs_f64() / 1000.0);

    Ok(())
}
