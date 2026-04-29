//! Bring-up: exchange UCX `WorkerAddress`es over a TCP rendezvous so that each
//! rank can build endpoints to every other rank.
//!
//! The protocol:
//! 1. rank 0 listens on `(host, port)`.
//! 2. ranks 1..N connect, each sends `(rank, addr_bytes)` length-prefixed.
//! 3. rank 0 collects all addresses, then broadcasts the full address list to
//!    every connected rank.
//! 4. Every rank then constructs endpoints to all peers (skipping itself).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::time::Duration;

use pluvio_ucx::{Worker, WorkerAddressInner};
use pluvio_ucx::worker::endpoint::Endpoint;

use crate::error::CollectiveError;

/// Bootstrap configuration. Typically derived from environment variables
/// `PLUVIO_COLL_RANK`, `PLUVIO_COLL_SIZE`, `PLUVIO_COLL_ROOT_HOST`,
/// `PLUVIO_COLL_ROOT_PORT` (the example sets these from `mpiexec`).
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub rank: usize,
    pub size: usize,
    pub root_host: String,
    pub root_port: u16,
}

impl BootstrapConfig {
    pub fn from_env() -> Result<Self, CollectiveError> {
        fn need(key: &str) -> Result<String, CollectiveError> {
            std::env::var(key).map_err(|_| {
                CollectiveError::Protocol("required environment variable not set")
            })
        }
        // Allow OpenMPI / MPICH PMI variables as fallback.
        let rank: usize = std::env::var("PLUVIO_COLL_RANK")
            .or_else(|_| std::env::var("OMPI_COMM_WORLD_RANK"))
            .or_else(|_| std::env::var("PMI_RANK"))
            .map_err(|_| CollectiveError::Protocol("rank not provided"))?
            .parse()
            .map_err(|_| CollectiveError::Protocol("rank parse error"))?;
        let size: usize = std::env::var("PLUVIO_COLL_SIZE")
            .or_else(|_| std::env::var("OMPI_COMM_WORLD_SIZE"))
            .or_else(|_| std::env::var("PMI_SIZE"))
            .map_err(|_| CollectiveError::Protocol("size not provided"))?
            .parse()
            .map_err(|_| CollectiveError::Protocol("size parse error"))?;
        let root_host = need("PLUVIO_COLL_ROOT_HOST").unwrap_or_else(|_| "localhost".to_string());
        let root_port: u16 = std::env::var("PLUVIO_COLL_ROOT_PORT")
            .unwrap_or_else(|_| "13337".to_string())
            .parse()
            .map_err(|_| CollectiveError::Protocol("port parse error"))?;
        Ok(Self {
            rank,
            size,
            root_host,
            root_port,
        })
    }
}

fn write_u32_len_prefixed(stream: &mut TcpStream, buf: &[u8]) -> std::io::Result<()> {
    let len = buf.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(buf)?;
    Ok(())
}

fn read_u32_len_prefixed(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// Synchronously exchange every rank's worker address. Returns the full
/// address vector indexed by rank.
fn exchange_addresses(
    config: &BootstrapConfig,
    my_addr: &[u8],
) -> Result<Vec<Vec<u8>>, CollectiveError> {
    let bind_addr = format!("0.0.0.0:{}", config.root_port);
    if config.rank == 0 {
        let listener = TcpListener::bind(&bind_addr)?;
        listener.set_nonblocking(false)?;

        let mut conns: HashMap<usize, TcpStream> = HashMap::new();
        let mut addresses: Vec<Vec<u8>> = vec![Vec::new(); config.size];
        addresses[0] = my_addr.to_vec();

        for _ in 1..config.size {
            let (mut stream, peer) = listener.accept()?;
            stream.set_nodelay(true)?;
            let mut hdr = [0u8; 4];
            stream.read_exact(&mut hdr)?;
            let peer_rank = u32::from_le_bytes(hdr) as usize;
            let peer_addr = read_u32_len_prefixed(&mut stream)?;
            tracing::debug!(
                "bootstrap: rank0 accepted rank={} from {}",
                peer_rank,
                peer
            );
            addresses[peer_rank] = peer_addr;
            conns.insert(peer_rank, stream);
        }

        // Broadcast the full address list back to every rank.
        for rank in 1..config.size {
            let mut stream = conns.remove(&rank).expect("missing connection");
            let count = config.size as u32;
            stream.write_all(&count.to_le_bytes())?;
            for addr in &addresses {
                write_u32_len_prefixed(&mut stream, addr)?;
            }
        }
        Ok(addresses)
    } else {
        // connect with retry
        let connect_addr = format!("{}:{}", config.root_host, config.root_port);
        let mut stream = None;
        for _ in 0..200 {
            match TcpStream::connect(&connect_addr) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let mut stream = stream.ok_or(CollectiveError::Protocol(
            "bootstrap: failed to connect to rank 0",
        ))?;
        stream.set_nodelay(true)?;
        let rank = config.rank as u32;
        stream.write_all(&rank.to_le_bytes())?;
        write_u32_len_prefixed(&mut stream, my_addr)?;

        // Receive the broadcast.
        let mut count_buf = [0u8; 4];
        stream.read_exact(&mut count_buf)?;
        let count = u32::from_le_bytes(count_buf) as usize;
        if count != config.size {
            return Err(CollectiveError::Protocol("bootstrap: size mismatch"));
        }
        let mut addresses = Vec::with_capacity(count);
        for _ in 0..count {
            addresses.push(read_u32_len_prefixed(&mut stream)?);
        }
        Ok(addresses)
    }
}

/// Resolves the rank's `Endpoint`s after exchanging addresses. Returns a
/// rank-indexed vector with `None` at the local rank.
pub fn bootstrap_communicator(
    worker: &Rc<Worker>,
    config: &BootstrapConfig,
) -> Result<Vec<Option<Rc<Endpoint>>>, CollectiveError> {
    let my_addr = worker
        .address()
        .map_err(|e| CollectiveError::Ucx(format!("Worker::address: {:?}", e)))?;
    let my_addr_bytes = my_addr.as_ref().to_vec();
    drop(my_addr);

    let addresses = exchange_addresses(config, &my_addr_bytes)?;
    let mut endpoints: Vec<Option<Rc<Endpoint>>> = Vec::with_capacity(config.size);
    for (rank, addr_bytes) in addresses.iter().enumerate() {
        if rank == config.rank {
            endpoints.push(None);
            continue;
        }
        let addr_inner = WorkerAddressInner::from(addr_bytes.as_slice());
        let ep = worker
            .connect_addr(&addr_inner)
            .map_err(|e| CollectiveError::Ucx(format!("connect_addr({}): {:?}", rank, e)))?;
        endpoints.push(Some(Rc::new(ep)));
    }
    Ok(endpoints)
}
