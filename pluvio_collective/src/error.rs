use thiserror::Error;

/// Errors produced by collective operations.
#[derive(Debug, Error)]
pub enum CollectiveError {
    #[error("MPI call failed with status code {0}")]
    Mpi(i32),
    #[error("UCX error: {0}")]
    Ucx(String),
    #[error("buffer length is not divisible by communicator size")]
    BadShape,
    #[error("bootstrap I/O failure: {0}")]
    Bootstrap(#[from] std::io::Error),
    #[error("invalid AM header (size {got} bytes, expected {expected})")]
    BadHeader { got: usize, expected: usize },
    #[error("collective protocol error: {0}")]
    Protocol(&'static str),
}
