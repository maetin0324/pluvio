//! Crate level module for the `pluvio` asynchronous runtime.
//!
//! This library provides a small runtime built around `io_uring` and
//! a simple task system.  It exposes modules for the executor, the I/O
//! reactor and task utilities.
//!
//! # Thread-Local Storage (TLS) Based APIs
//!
//! This library provides two sets of APIs:
//!
//! 1. **TLS-based APIs**: Convenient functions that use thread-local storage.
//!    These include `spawn()`, `run()`, etc. You must call `set_runtime()` first.
//!
//! 2. **Explicit Runtime APIs**: Methods that require an explicit `Runtime` reference.
//!    These have `_with_runtime` or `_and_runtime` suffixes (e.g., `spawn_with_runtime()`).
//!
//! ## Example: TLS-based API
//!
//! ```no_run
//! use pluvio_runtime::executor::{Runtime, set_runtime, spawn, run};
//! use std::rc::Rc;
//!
//! let runtime = Runtime::new(1024);
//! set_runtime(runtime.clone());
//!
//! spawn(async {
//!     println!("Hello from a spawned task!");
//! });
//!
//! run(async {
//!     println!("Running main task");
//! });
//! ```
//!
//! ## Example: Explicit Runtime API
//!
//! ```no_run
//! use pluvio_runtime::executor::Runtime;
//!
//! let runtime = Runtime::new(1024);
//! runtime.spawn_with_runtime(async {
//!     println!("Hello from a spawned task!");
//! });
//!
//! runtime.run_with_runtime(async {
//!     println!("Running main task");
//! });
//! ```

/// Task executor and runtime utilities.
pub mod executor;
/// Reactor implementation based on `io_uring`.
pub mod reactor;
/// Task abstraction and waker utilities.
pub mod task;
/// Track ID management for Perfetto trace visualization.
pub mod track;

// Re-export TLS-based convenience functions at the crate root for easier access
pub use executor::{
    clear_runtime, get_runtime, run, run_with_name, set_runtime, spawn, spawn_polling,
    spawn_polling_with_name, spawn_with_name,
};

// Re-export track ID management functions
pub use track::{
    acquire_track_id, get_current_track_id, maybe_tracked, release_track_id, set_current_track_id,
    tracked, MaybeTracked, TrackedFuture,
};
