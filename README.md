# Pluvio

Pluvio is a high-performance asynchronous runtime for Rust built around modern I/O technologies: Linux `io_uring` for efficient file I/O, UCX (Unified Communication X) for high-performance networking, and `mpi-sys` for non-blocking MPI collectives.

## Overview

Pluvio provides a lightweight, single-threaded async runtime designed for applications requiring high-throughput I/O operations. It features a custom task scheduler, reactor-based I/O handling, and direct integration with `io_uring`, UCX, and MPI for optimal performance. The whole runtime is `Rc`/`RefCell`-based — no `Arc`, no `Mutex` — and assumes one executor per OS thread.

## Architecture

The project is organized as a Cargo workspace with the following members:

### 1. pluvio_runtime

The core runtime that provides:
- **Task Executor**: Schedules and manages asynchronous tasks
- **Reactor System**: Pluggable reactor interface (`fn poll(&self)` / `fn status(&self) -> ReactorStatus`) for different I/O backends
- **Task Abstraction**: Custom task and waker implementation for efficient async execution
- **Idle parking**: Conservative `sched_yield` / `nanosleep` backoff while quiescent, tunable via `PLUVIO_IDLE_PARK`, `PLUVIO_IDLE_YIELD_AFTER`, `PLUVIO_IDLE_SLEEP_AFTER`
- **Select / timeout combinators**: `select`, `Either`, `timeout` for composing futures without depending on a third-party runtime

Key features:
- Single-threaded executor with CPU affinity support
- Efficient task scheduling using SPSC queues
- Reactor registration system for composable I/O backends with per-reactor status caching to keep the hot path cheap

### 2. pluvio_uring

An `io_uring` based I/O reactor providing:
- **DMA file operations** (`O_DIRECT`, fixed buffers) with an async open / read / write / fallocate API
- **Pre-registered buffer allocator** for zero-copy operations
- **`IoUringReactorBuilder`** for tuning queue size, submission depth, SQPOLL, and wait timeouts

### 3. pluvio_ucx

A UCX reactor for high-performance networking:
- **Worker / Endpoint / Context** wrappers around `async-ucx`
- **Active Message** send and `am_stream`-based pull-style receive
- **Listener-based and address-based** connection establishment
- **Active-op atomic counter** so `Reactor::status()` doesn't iterate registered workers on the hot path

### 4. pluvio_timer

A small timer reactor used for `Delay`, `sleep`, and `timeout`. Drives a min-heap of registered wakers.

### 5. pluvio_collective

Non-blocking collective communication (`Allreduce`, `Scatter`) on top of the Pluvio reactor model. Two backends share one trait so application code can swap between them:

- **`mpi_backend`**: wraps `MPI_Iallreduce` / `MPI_Iscatter` via `mpi-sys` and an `MpiReactor` that drains in-flight requests with `MPI_Test`
- **`ucx_backend`**: ring allreduce and direct-send scatter on top of `pluvio_ucx` Active Messages, with TCP-based bootstrap of UCX worker addresses

See [`pluvio_collective/README.md`](pluvio_collective/README.md) for usage and limitations.

## Usage

### Basic Runtime Setup

```rust
use pluvio_runtime::executor::Runtime;

// Create a runtime with task queue size
let runtime = Runtime::new(1024);

// Set CPU affinity (optional)
runtime.set_affinity(0);

// Register reactors
runtime.register_reactor("my_reactor", reactor);

// Run async tasks: blocks until the spawned future and all background tasks finish
runtime.clone().run_with_name_and_runtime("main", async move {
    // Your async code here
});
```

The TLS-based shorthand (`pluvio_runtime::run`, `spawn`, etc.) is also re-exported at the crate root once `set_runtime` has been called.

### io_uring Example

```rust
use pluvio_runtime::executor::Runtime;
use pluvio_uring::reactor::IoUringReactor;
use pluvio_uring::file::DmaFile;
use std::time::Duration;

let runtime = Runtime::new(1024);
let reactor = IoUringReactor::builder()
    .queue_size(2048)
    .buffer_size(1 << 20)  // 1 MiB
    .submit_depth(64)
    .wait_submit_timeout(Duration::from_millis(100))
    .wait_complete_timeout(Duration::from_millis(150))
    .build();

runtime.register_reactor("io_uring_reactor", reactor);

runtime.clone().run_with_runtime(async move {
    let file = std::rc::Rc::new(DmaFile::new(file));
    let buffer = file.acquire_buffer().await;
    file.write_fixed(buffer, offset).await;
});
```

### UCX Example

```rust
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{Context, UCXReactor};

let runtime = Runtime::new(1024);
let ucx_reactor = UCXReactor::current();
runtime.register_reactor("ucx_reactor", ucx_reactor.clone());

runtime.clone().run_with_runtime(async move {
    let context = Context::new().unwrap();
    let worker = context.create_worker().unwrap();

    let endpoint = worker.connect_addr(&worker_addr).unwrap();
    endpoint.am_send(id, &header, &data, true, None).await.unwrap();
});
```

### Collective (MPI Allreduce) Example

```rust
use pluvio_collective::mpi_backend::{disable_async_progress, MpiCommunicator, MpiReactor};
use pluvio_collective::{Collective, Sum};
use pluvio_runtime::executor::Runtime;

disable_async_progress();
unsafe { mpi_sys::MPI_Init_thread(/* ... */); }

let runtime = Runtime::new(1024);
let reactor = MpiReactor::new();
runtime.register_reactor("mpi", reactor.clone());
let comm = MpiCommunicator::world(reactor).unwrap();

runtime.clone().run_with_name_and_runtime("allreduce", async move {
    let mut buf = vec![comm.rank() as f32 + 1.0; 1024];
    comm.allreduce::<Sum>(&mut buf).await.unwrap();
});

unsafe { mpi_sys::MPI_Finalize(); }
```

The UCX-only ring allreduce is structured the same way but bootstraps its own endpoints with `pluvio_collective::ucx_backend::bootstrap_communicator`.

## Building

```bash
cargo build --workspace
```

The `pluvio_collective` crate has `default-features = ["mpi", "ucx"]`. Either feature can be turned off individually:

```bash
cargo build -p pluvio_collective --no-default-features --features ucx   # UCX only
cargo build -p pluvio_collective --no-default-features --features mpi   # MPI only
```

`mpi-sys` invokes `mpicc` at build time, so an MPI implementation must be on `PATH` when the `mpi` feature is enabled.

## Examples

The repository ships several examples:

- `examples/uring_example`: high-throughput file I/O using io_uring
- `examples/ucx_example`: UCX-based networking with Active Messages
- `examples/remoteio_example`: combined io_uring + UCX remote I/O
- `examples/mpi_example`: MPI + UCX remote READ benchmark (excluded from the workspace; build with `cargo build --manifest-path examples/mpi_example/Cargo.toml`)
- `pluvio_collective/examples/coll_mpi_example.rs`: minimal MPI `Allreduce` driven by Pluvio
- `pluvio_collective/examples/coll_ucx_example.rs`: ring allreduce on top of UCX AM

Run the workspace examples directly:

```bash
cargo run --example uring_example
cargo run --example ucx_example
mpiexec -n 2 cargo run --example coll_mpi_example -p pluvio_collective
mpiexec -n 2 \
  -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 -x PLUVIO_COLL_ROOT_PORT=14200 \
  cargo run --example coll_ucx_example -p pluvio_collective
```

## Tests

Unit tests are runnable without any external launcher:

```bash
cargo test --workspace --lib
```

Integration tests for `pluvio_collective` are gated behind `#[ignore]` because they need a multi-process launcher. The driver script handles the build and `mpiexec` invocation:

```bash
scripts/run_integration_tests.sh                  # 2 ranks, debug build
PROCS=4 PORT=14100 PROFILE=release scripts/run_integration_tests.sh
```

The script honours `CARGO_TARGET_DIR` when set; otherwise it defaults to `./target`.

## Requirements

- Rust 2024 edition (rustc 1.90.0 or later)
- Linux kernel with `io_uring` support (5.1+)
- UCX **1.18 or newer** for `pluvio_ucx` (the vendored async-ucx targets the 1.18 ABI; running against an older `libucp.so.0` will produce assertion failures on shutdown). On Ubuntu 24.04 the system package is 1.16, so install a newer UCX system-wide (e.g. drop a config under `/etc/ld.so.conf.d/`) or copy the build from `target/<profile>/build/ucx1-sys-*/out/` to a stable path.
- An MPI implementation (OpenMPI 4.x / 5.x or MPICH 4.x) for `pluvio_collective`'s `mpi` feature and `examples/mpi_example`. The `MPICH_ASYNC_PROGRESS=0`, `MPIR_CVAR_ASYNC_PROGRESS=0`, `OMPI_MCA_pml_ucx_progress_iterations=0` knobs should be set so that MPI's internal progress threads don't fight Pluvio's reactor; `pluvio_collective::mpi_backend::disable_async_progress()` does this for you.

## License

This project is under development and license information is to be determined.

## Performance Characteristics

Pluvio is designed for:
- High-throughput sequential I/O workloads
- Low-latency message passing in HPC environments
- Non-blocking collective communication that stays inside the user's event loop instead of being driven by an MPI-internal progress thread
- Applications requiring fine-grained control over I/O scheduling
- Single-threaded async applications with CPU affinity requirements
