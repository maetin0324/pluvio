//! Crate level module for the `pluvio` asynchronous runtime.
//!
//! This library provides a small runtime built around `io_uring` and
//! a simple task system.  It exposes modules for the executor, the I/O
//! reactor and task utilities.

/// Task executor and runtime utilities.
pub mod executor;
/// Asynchronous file and buffer helpers.
pub mod io;
/// Reactor implementation based on `io_uring`.
pub mod reactor;
/// Task abstraction and waker utilities.
pub mod task;

