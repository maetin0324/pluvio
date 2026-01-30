// MPI + UCX Remote READ Benchmark
// This example simulates IOR-like remote read operations to isolate RPC READ performance
//
// Usage:
//   mpirun ... mpi_example server <registry_dir> <data_dir>
//   mpirun ... mpi_example client <registry_dir> <num_servers>
//
// Server behavior:
//   1. Create a test file at startup
//   2. Write initial data to the file
//   3. Wait for READ requests from clients
//   4. Read from local file using io_uring
//   5. Send data back to client via AM reply
//
// Client behavior:
//   1. Connect to assigned server (round-robin)
//   2. Send READ requests (offset, length)
//   3. Receive data in AM reply
//   4. Measure performance

use pluvio_runtime::executor::Runtime;
use pluvio_ucx::UCXReactor;
use pluvio_uring::file::DmaFile;
use pluvio_uring::reactor::{IoUringReactor, register_file};
use std::rc::Rc;
use std::cell::RefCell;
use mpi::traits::*;
use std::time::{Duration, Instant};
use std::path::PathBuf;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

// Test parameters (configurable via environment variables)
fn get_data_size() -> usize {
    std::env::var("DATA_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4 << 20) // Default: 4 MiB (matching BenchFS chunk size)
}

fn get_num_transfers() -> usize {
    std::env::var("NUM_TRANSFERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256) // Default: 256 transfers
}

fn get_window_size() -> usize {
    std::env::var("WINDOW_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32) // Default: 32 concurrent requests
}

// AM IDs
const READ_REQUEST_AM_ID: u16 = 20;
const READ_RESPONSE_AM_ID: u32 = 21;

// Request header: offset and length
#[repr(C)]
#[derive(Clone, Copy)]
struct ReadRequestHeader {
    offset: u64,
    length: u64,
    request_id: u64,
}

// Response header: status and bytes_read
#[repr(C)]
#[derive(Clone, Copy)]
struct ReadResponseHeader {
    request_id: u64,
    bytes_read: u64,
    status: i32,
    _padding: i32,
}

#[derive(Debug, Clone, PartialEq)]
enum Role {
    Server,
    Client,
}

fn print_usage(program: &str) {
    eprintln!("Usage:");
    eprintln!("  Server: {} server <registry_dir> <data_dir>", program);
    eprintln!("  Client: {} client <registry_dir> <num_servers>", program);
    eprintln!();
    eprintln!("Environment variables:");
    eprintln!("  DATA_SIZE     - Bytes per transfer (default: 4MiB)");
    eprintln!("  NUM_TRANSFERS - Number of transfers per client (default: 256)");
    eprintln!("  WINDOW_SIZE   - Concurrent requests window (default: 32)");
}

fn main() {
    // Initialize MPI first
    let universe = mpi::initialize().unwrap();
    let world = universe.world();
    let mpi_rank = world.rank();
    let mpi_size = world.size();

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        if mpi_rank == 0 {
            print_usage(&args[0]);
        }
        std::process::exit(1);
    }

    let role = match args[1].as_str() {
        "server" => Role::Server,
        "client" => Role::Client,
        _ => {
            if mpi_rank == 0 {
                eprintln!("Unknown role: {}. Use 'server' or 'client'.", args[1]);
                print_usage(&args[0]);
            }
            std::process::exit(1);
        }
    };

    let registry_dir = PathBuf::from(&args[2]);

    // Role-specific arguments
    let (data_dir, num_servers) = match role {
        Role::Server => {
            if args.len() < 4 {
                if mpi_rank == 0 {
                    eprintln!("Server requires data_dir argument");
                    print_usage(&args[0]);
                }
                std::process::exit(1);
            }
            (Some(PathBuf::from(&args[3])), 0)
        }
        Role::Client => {
            if args.len() < 4 {
                if mpi_rank == 0 {
                    eprintln!("Client requires num_servers argument");
                    print_usage(&args[0]);
                }
                std::process::exit(1);
            }
            let num_servers: i32 = args[3].parse().unwrap_or_else(|_| {
                if mpi_rank == 0 {
                    eprintln!("Invalid num_servers: {}", args[3]);
                }
                std::process::exit(1);
            });
            (None, num_servers)
        }
    };

    // Get parameters
    let data_size = get_data_size();
    let num_transfers = get_num_transfers();
    let window_size = get_window_size();
    let total_size = data_size * num_transfers;

    // Setup logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    // Create registry directory (rank 0 only)
    if mpi_rank == 0 {
        std::fs::create_dir_all(&registry_dir).ok();
        println!("========================================");
        println!("MPI + UCX Remote READ Benchmark");
        println!("========================================");
        println!("Role: {:?}", role);
        println!("MPI processes: {}", mpi_size);
        println!("Registry directory: {:?}", registry_dir);
        if let Some(ref dd) = data_dir {
            println!("Data directory: {:?}", dd);
        }
        if role == Role::Client {
            println!("Number of servers: {}", num_servers);
        }
        println!("Data size per transfer: {} MiB", data_size / (1 << 20));
        println!("Transfers per client: {}", num_transfers);
        println!("Total size per client: {} MiB", total_size / (1 << 20));
        println!("Window size: {}", window_size);
        println!("========================================");
    }

    // MPI barrier to ensure directory is created
    world.barrier();

    // Create pluvio runtime
    let runtime = Runtime::new(1024);
    pluvio_runtime::set_runtime(runtime.clone());

    // Create io_uring reactor only for servers (to avoid OOM with many clients)
    let uring_reactor = if role == Role::Server {
        let reactor = IoUringReactor::builder()
            .queue_size(2048)
            .buffer_size(data_size)
            .submit_depth(64)
            .wait_submit_timeout(Duration::from_millis(1))
            .wait_complete_timeout(Duration::from_millis(1))
            .build();
        runtime.register_reactor("io_uring", reactor.clone());
        Some(reactor)
    } else {
        None
    };

    // Create UCX reactor and context
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx", ucx_reactor.clone());

    // Run benchmark
    let result = run_benchmark(
        runtime.clone(),
        uring_reactor,
        ucx_reactor.clone(),
        mpi_rank,
        mpi_size,
        role.clone(),
        &registry_dir,
        data_dir.as_ref(),
        num_servers,
        data_size,
        num_transfers,
        window_size,
    );

    match result {
        Ok(_) => {
            if mpi_rank == 0 {
                println!("✓ {:?} benchmark completed successfully!", role);
            }
        }
        Err(e) => {
            eprintln!("✗ Rank {}: Benchmark failed: {}", mpi_rank, e);
        }
    }

    // Final MPI barrier before finalization
    world.barrier();

    if mpi_rank == 0 {
        println!("{:?} complete. All ranks finished.", role);
    }
}

fn run_benchmark(
    runtime: Rc<Runtime>,
    uring_reactor: Option<Rc<IoUringReactor>>,
    reactor: Rc<UCXReactor>,
    mpi_rank: i32,
    mpi_size: i32,
    role: Role,
    registry_dir: &PathBuf,
    data_dir: Option<&PathBuf>,
    num_servers: i32,
    data_size: usize,
    num_transfers: usize,
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create UCX context and worker
    let ucx_context = Rc::new(pluvio_ucx::Context::new()?);
    let worker = ucx_context.create_worker()?;
    reactor.register_worker(worker.clone());

    println!("Rank {}: UCX worker created successfully", mpi_rank);

    // Clone paths for async block
    let registry_dir = registry_dir.clone();
    let data_dir = data_dir.cloned();

    // Run based on role
    pluvio_runtime::run_with_name("read_benchmark", async move {
        match role {
            Role::Server => {
                let uring = uring_reactor.expect("Server requires io_uring reactor");
                run_server(
                    runtime,
                    uring,
                    worker,
                    mpi_rank,
                    registry_dir,
                    data_dir.unwrap(),
                    data_size,
                    num_transfers,
                ).await
            }
            Role::Client => {
                run_client(
                    worker,
                    mpi_rank,
                    mpi_size,
                    registry_dir,
                    num_servers,
                    data_size,
                    num_transfers,
                    window_size,
                ).await
            }
        }
    });

    Ok(())
}

async fn run_server(
    _runtime: Rc<Runtime>,
    _uring_reactor: Rc<IoUringReactor>,
    worker: Rc<pluvio_ucx::Worker>,
    mpi_rank: i32,
    registry_dir: PathBuf,
    data_dir: PathBuf,
    data_size: usize,
    num_transfers: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let total_size = data_size * num_transfers;

    println!("Rank {}: Starting as server", mpi_rank);

    // Calculate deterministic port (use mpi_rank as server index)
    let server_index = mpi_rank;
    let port = 20000 + server_index as u16;
    let bind_addr = format!("0.0.0.0:{}", port).parse().unwrap();

    // Create test file and write initial data
    let target_file = data_dir.join(format!("read_bench_server_{}.data", mpi_rank));
    println!("Rank {}: Creating test file: {:?} ({} MiB)", mpi_rank, target_file, total_size / (1 << 20));

    let file = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(&target_file)?;
    let fd = file.as_raw_fd();
    file.set_len(total_size as u64)?;
    register_file(fd);

    let dma_file = Rc::new(DmaFile::new(file));
    dma_file.fallocate(total_size as u64).await?;

    // Write initial data to file (so we have something to read)
    println!("Rank {}: Writing initial data to file...", mpi_rank);
    let write_start = Instant::now();

    for i in 0..num_transfers {
        let mut buffer = dma_file.acquire_buffer().await;
        let buf_slice = buffer.as_mut_slice();
        // Fill with pattern based on offset
        let pattern = (i & 0xFF) as u8;
        buf_slice.fill(pattern);

        let offset = i * data_size;
        dma_file.write_fixed(buffer, offset as u64).await?;
    }

    let write_elapsed = write_start.elapsed();
    println!("Rank {}: Initial data written in {:.2}s ({:.2} MB/s)",
             mpi_rank, write_elapsed.as_secs_f64(),
             total_size as f64 / write_elapsed.as_secs_f64() / (1 << 20) as f64);

    // Create listener
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

    // Save socket address for client connection
    let socket_info = format!("{}:{}", hostname, port);
    let socket_file = registry_dir.join(format!("server_{}.socket", server_index));
    std::fs::write(&socket_file, &socket_info)?;
    println!("Rank {}: Socket info saved: {}", mpi_rank, socket_info);

    // Create AM stream for requests
    let stream = Rc::new(worker.am_stream(READ_REQUEST_AM_ID)?);

    // Accept connection
    println!("Rank {}: Waiting for client connection", mpi_rank);
    let conn_request = listener.next().await;
    let endpoint = worker.accept(conn_request).await?;
    let endpoint = Rc::new(endpoint);
    println!("Rank {}: Accepted client connection", mpi_rank);

    // Exchange WorkerAddress for InfiniBand lanes
    println!("Rank {}: Exchanging WorkerAddress", mpi_rank);

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
    endpoint.stream_send(&addr_len.to_le_bytes()).await?;
    endpoint.stream_send(our_addr_bytes).await?;

    println!("Rank {}: WorkerAddress exchange complete", mpi_rank);

    // Handle READ requests
    let mut total_bytes_read = 0usize;
    let mut total_io_time_us = 0u64;
    let start_time = Instant::now();

    // Track per-request timing
    let mut request_times: Vec<u64> = Vec::with_capacity(num_transfers);

    for i in 0..num_transfers {
        let request_start = Instant::now();

        // Wait for READ request
        let msg = stream.wait_msg().await.ok_or("Stream closed")?;
        let header = msg.header();

        // Parse request header
        let offset = u64::from_le_bytes(header[0..8].try_into().unwrap()) as usize;
        let length = u64::from_le_bytes(header[8..16].try_into().unwrap()) as usize;
        let request_id = u64::from_le_bytes(header[16..24].try_into().unwrap());

        // Read from file
        let io_start = Instant::now();
        let buffer = dma_file.acquire_buffer().await;
        let (_bytes_read, mut buffer) = dma_file.read_fixed(buffer, offset as u64).await?;
        let io_elapsed = io_start.elapsed();
        total_io_time_us += io_elapsed.as_micros() as u64;

        // Send response with data
        let response_header = ReadResponseHeader {
            request_id,
            bytes_read: length as u64,
            status: 0,
            _padding: 0,
        };
        let header_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &response_header as *const _ as *const u8,
                std::mem::size_of::<ReadResponseHeader>(),
            )
        };

        let data_slice = &buffer.as_mut_slice()[..length];

        unsafe {
            msg.reply(
                READ_RESPONSE_AM_ID,
                header_bytes,
                data_slice,
                false,
                Some(pluvio_ucx::async_ucx::ucp::AmProto::Rndv),
            ).await?;
        }

        total_bytes_read += length;
        let request_elapsed = request_start.elapsed();
        request_times.push(request_elapsed.as_micros() as u64);

        if (i + 1) % 64 == 0 {
            println!("Rank {}: Processed {}/{} READ requests (io_time: {} us)",
                     mpi_rank, i + 1, num_transfers, io_elapsed.as_micros());
        }
    }

    let elapsed = start_time.elapsed();

    // Calculate statistics
    request_times.sort();
    let avg_request_us = request_times.iter().sum::<u64>() / request_times.len() as u64;
    let p50 = request_times[request_times.len() / 2];
    let p95 = request_times[request_times.len() * 95 / 100];
    let p99 = request_times[request_times.len() * 99 / 100];

    println!("========================================");
    println!("Rank {}: SERVER Summary", mpi_rank);
    println!("========================================");
    println!("Total bytes read: {} MiB", total_bytes_read / (1 << 20));
    println!("Elapsed time: {:.2} s", elapsed.as_secs_f64());
    println!("Bandwidth: {:.2} MB/s", total_bytes_read as f64 / elapsed.as_secs_f64() / (1 << 20) as f64);
    println!("IOPS: {:.2} KIOPS", num_transfers as f64 / elapsed.as_secs_f64() / 1000.0);
    println!("Total I/O time: {} ms", total_io_time_us / 1000);
    println!("Avg I/O time per request: {} us", total_io_time_us / num_transfers as u64);
    println!("Request latency (avg/p50/p95/p99): {} / {} / {} / {} us",
             avg_request_us, p50, p95, p99);
    println!("========================================");

    Ok(())
}

async fn run_client(
    worker: Rc<pluvio_ucx::Worker>,
    mpi_rank: i32,
    _mpi_size: i32,
    registry_dir: PathBuf,
    num_servers: i32,
    data_size: usize,
    num_transfers: usize,
    window_size: usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Rank {}: Starting as client", mpi_rank);

    // Determine which server to connect to (round-robin)
    let server_index = mpi_rank % num_servers;

    println!("Rank {}: Connecting to server index {}", mpi_rank, server_index);

    // Wait for server socket file
    let server_socket_file = registry_dir.join(format!("server_{}.socket", server_index));
    println!("Rank {}: Waiting for socket file: {:?}", mpi_rank, server_socket_file);

    let mut retries = 0;
    while !server_socket_file.exists() && retries < 120 {
        std::thread::sleep(Duration::from_millis(100));
        retries += 1;
    }

    if !server_socket_file.exists() {
        return Err(format!("Rank {}: Server socket file not found after {} retries",
                          mpi_rank, retries).into());
    }

    // Read server socket address
    let server_socket_str = std::fs::read_to_string(&server_socket_file)?;

    // Resolve hostname
    use std::net::ToSocketAddrs;
    let mut addrs = server_socket_str.trim()
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve server socket: {}", e))?;

    let server_socket = addrs.next()
        .ok_or_else(|| format!("No addresses resolved"))?;

    println!("Rank {}: Connecting to {}", mpi_rank, server_socket);
    let endpoint = worker.connect_socket(server_socket).await?;
    println!("Rank {}: Connected to server", mpi_rank);

    // Exchange WorkerAddress
    let our_address = worker.address()?;
    let our_addr_bytes: &[u8] = our_address.as_ref();
    let addr_len = our_addr_bytes.len() as u32;
    endpoint.stream_send(&addr_len.to_le_bytes()).await?;
    endpoint.stream_send(our_addr_bytes).await?;

    // Receive server's WorkerAddress
    let mut len_buf: Vec<std::mem::MaybeUninit<u8>> = vec![std::mem::MaybeUninit::uninit(); 4];
    endpoint.stream_recv(&mut len_buf).await?;
    let len_buf: Vec<u8> = unsafe { std::mem::transmute(len_buf) };
    let server_addr_len = u32::from_le_bytes([len_buf[0], len_buf[1], len_buf[2], len_buf[3]]) as usize;

    let mut server_addr_bytes: Vec<std::mem::MaybeUninit<u8>> = vec![std::mem::MaybeUninit::uninit(); server_addr_len];
    endpoint.stream_recv(&mut server_addr_bytes).await?;

    println!("Rank {}: WorkerAddress exchange complete", mpi_rank);

    let endpoint = Rc::new(endpoint);

    // Create AM stream for responses
    let stream = Rc::new(worker.am_stream(READ_RESPONSE_AM_ID as u16)?);

    // Allocate receive buffer
    let recv_buffer = Rc::new(RefCell::new(vec![0u8; data_size]));

    // Send READ requests with pipelining
    let mut inflight = 0usize;
    let mut total_received = 0usize;
    let mut request_times: Vec<u64> = Vec::with_capacity(num_transfers);
    let start_time = Instant::now();

    for i in 0..num_transfers {
        // Wait if window is full
        while inflight >= window_size {
            let request_start = Instant::now();
            let mut msg = stream.wait_msg().await.ok_or("Stream closed")?;

            // Receive data into buffer
            {
                let mut buf = recv_buffer.borrow_mut();
                msg.recv_data_single(&mut buf[..]).await?;
            }

            let request_elapsed = request_start.elapsed();
            request_times.push(request_elapsed.as_micros() as u64);

            total_received += data_size;
            inflight -= 1;
        }

        // Calculate offset (simulate sequential access)
        let offset = i * data_size;

        // Create request header
        let header = ReadRequestHeader {
            offset: offset as u64,
            length: data_size as u64,
            request_id: i as u64,
        };
        let header_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                &header as *const _ as *const u8,
                std::mem::size_of::<ReadRequestHeader>(),
            )
        };

        // Send READ request (no data, just header)
        endpoint
            .am_send(
                READ_REQUEST_AM_ID as u32,
                header_bytes,
                &[],  // No data in request
                true,
                None, // Eager for small header
            )
            .await?;

        inflight += 1;

        if (i + 1) % 64 == 0 {
            println!("Rank {}: Sent {}/{} READ requests", mpi_rank, i + 1, num_transfers);
        }
    }

    // Wait for remaining responses
    while inflight > 0 {
        let request_start = Instant::now();
        let mut msg = stream.wait_msg().await.ok_or("Stream closed")?;

        {
            let mut buf = recv_buffer.borrow_mut();
            msg.recv_data_single(&mut buf[..]).await?;
        }

        let request_elapsed = request_start.elapsed();
        request_times.push(request_elapsed.as_micros() as u64);

        total_received += data_size;
        inflight -= 1;
    }

    let elapsed = start_time.elapsed();

    // Calculate statistics
    request_times.sort();
    let avg_request_us = if request_times.is_empty() { 0 } else {
        request_times.iter().sum::<u64>() / request_times.len() as u64
    };
    let p50 = if request_times.is_empty() { 0 } else { request_times[request_times.len() / 2] };
    let p95 = if request_times.is_empty() { 0 } else { request_times[request_times.len() * 95 / 100] };
    let p99 = if request_times.is_empty() { 0 } else { request_times[request_times.len() * 99 / 100] };

    println!("========================================");
    println!("Rank {}: CLIENT Summary", mpi_rank);
    println!("========================================");
    println!("Total bytes received: {} MiB", total_received / (1 << 20));
    println!("Elapsed time: {:.2} s", elapsed.as_secs_f64());
    println!("Bandwidth: {:.2} MB/s", total_received as f64 / elapsed.as_secs_f64() / (1 << 20) as f64);
    println!("IOPS: {:.2} KIOPS", num_transfers as f64 / elapsed.as_secs_f64() / 1000.0);
    println!("Response latency (avg/p50/p95/p99): {} / {} / {} / {} us",
             avg_request_us, p50, p95, p99);
    println!("========================================");

    Ok(())
}
