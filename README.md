# Pluvio

Pluvio is a high-performance asynchronous runtime for Rust built around modern I/O technologies: Linux `io_uring` for efficient file I/O and UCX (Unified Communication X) for high-performance networking.

## Overview

Pluvio provides a lightweight, single-threaded async runtime designed for applications requiring high-throughput I/O operations. It features a custom task scheduler, reactor-based I/O handling, and direct integration with `io_uring` and UCX for optimal performance.

## Architecture

The project is organized into three main components:

### 1. pluvio_runtime

The core runtime that provides:
- **Task Executor**: Schedules and manages asynchronous tasks
- **Reactor System**: Pluggable reactor interface for different I/O backends
- **Task Abstraction**: Custom task and waker implementation for efficient async execution
- **Runtime Statistics**: Utilities to inspect running tasks and performance metrics

Key features:
- Single-threaded executor with CPU affinity support
- Efficient task scheduling using SPSC (Single Producer Single Consumer) queues
- Reactor registration system for composable I/O backends

### 2. pluvio_uring

An `io_uring` based I/O reactor providing:
- **DMA (Direct Memory Access) File Operations**: High-performance file I/O with O_DIRECT support
- **Fixed Buffer Management**: Pre-registered buffer allocator for zero-copy operations
- **Async File Operations**: read, write, fallocate operations with future-based API
- **Configurable Parameters**: Submission depth, queue size, and timeout settings

Key features:
- Zero-copy I/O using registered buffers
- Batch submission for improved throughput
- Configurable wait timeouts for submission and completion

### 3. pluvio_ucx

A UCX (Unified Communication X) reactor for high-performance networking:
- **Worker Management**: UCX worker lifecycle and state management
- **Endpoint Connections**: Socket-based and address-based connection establishment
- **Active Messages (AM)**: Low-latency message passing with streaming support
- **Listener Support**: Server-side connection acceptance

Key features:
- Integration with async-ucx for UCX operations
- Active Message streaming with configurable window sizes
- Efficient endpoint and connection management

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

// Run async tasks
runtime.clone().run(async move {
    // Your async code here
});
```

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

runtime.clone().run(async move {
    let file = std::rc::Rc::new(DmaFile::new(file));
    let buffer = file.acquire_buffer().await;
    file.write_fixed(buffer, offset).await;
});
```

### UCX Example

```rust
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{UCXReactor, Context};

let runtime = Runtime::new(1024);
let ucx_reactor = UCXReactor::current();
runtime.register_reactor("ucx_reactor", ucx_reactor.clone());

runtime.clone().run(async move {
    let context = Context::new().unwrap();
    let worker = context.create_worker().unwrap();
    ucx_reactor.register_worker(worker.clone());
    
    // Connect and send messages
    let endpoint = worker.connect_addr(&worker_addr).unwrap();
    endpoint.am_send(id, &header, &data, true, None).await.unwrap();
});
```

## Building

```bash
cargo build --workspace
```

## Examples

The repository includes examples demonstrating the usage:

- `examples/uring_example`: Demonstrates high-throughput file I/O using io_uring
- `examples/ucx_example`: Shows UCX-based networking with Active Messages

Run examples with:
```bash
cargo run --example uring_example
cargo run --example ucx_example
```

## Requirements

- Rust 2024 edition (rustc 1.90.0 or later)
- Linux kernel with io_uring support (5.1+)
- UCX library (for pluvio_ucx)

## License

This project is under development and license information is to be determined.

## Performance Characteristics

Pluvio is designed for:
- High-throughput sequential I/O workloads
- Low-latency message passing in HPC environments
- Applications requiring fine-grained control over I/O scheduling
- Single-threaded async applications with CPU affinity requirements
