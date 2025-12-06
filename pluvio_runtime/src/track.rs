//! Track ID management for Perfetto trace visualization.
//!
//! This module provides track ID allocation and management for separating
//! async tasks into different tracks in Perfetto traces.
//!
//! # Feature Flag
//!
//! This module is only available when the `perfetto-tracks` feature is enabled.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

/// Track ID allocator that manages a pool of track IDs.
///
/// When a task is spawned, it acquires a track ID from this allocator.
/// When the task completes, the ID is returned to the pool for reuse.
pub struct TrackIdAllocator {
    /// Pool of available (returned) track IDs
    available_ids: RefCell<VecDeque<u64>>,
    /// Next ID to allocate if pool is empty
    next_id: AtomicU64,
}

impl TrackIdAllocator {
    /// Create a new track ID allocator.
    pub const fn new() -> Self {
        Self {
            available_ids: RefCell::new(VecDeque::new()),
            next_id: AtomicU64::new(1), // Start from 1, 0 is reserved for main track
        }
    }

    /// Acquire a track ID. Returns a reused ID if available, otherwise allocates a new one.
    pub fn acquire(&self) -> u64 {
        if let Some(id) = self.available_ids.borrow_mut().pop_front() {
            id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        }
    }

    /// Release a track ID back to the pool for reuse.
    pub fn release(&self, id: u64) {
        if id != 0 {
            // Don't release the main track ID
            self.available_ids.borrow_mut().push_back(id);
        }
    }
}

impl Default for TrackIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// Global track ID allocator for the current thread
    static TRACK_ALLOCATOR: TrackIdAllocator = TrackIdAllocator::new();
    /// Current track ID for the executing task
    static CURRENT_TRACK_ID: RefCell<u64> = const { RefCell::new(0) };
}

/// Acquire a new track ID for a spawned task.
pub fn acquire_track_id() -> u64 {
    TRACK_ALLOCATOR.with(|alloc| alloc.acquire())
}

/// Release a track ID when a task completes.
pub fn release_track_id(id: u64) {
    TRACK_ALLOCATOR.with(|alloc| alloc.release(id));
}

/// Set the current track ID for subsequent spans.
pub fn set_current_track_id(id: u64) {
    CURRENT_TRACK_ID.with(|current| {
        *current.borrow_mut() = id;
    });
}

/// Get the current track ID.
pub fn get_current_track_id() -> u64 {
    CURRENT_TRACK_ID.with(|current| *current.borrow())
}

/// A future wrapper that assigns a unique track ID to the task.
///
/// When the future is first polled, it acquires a track ID and sets it as the
/// current track. When the future completes, the track ID is released back to
/// the pool for reuse.
pub struct TrackedFuture<F> {
    inner: F,
    track_id: Option<u64>,
}

impl<F: Future> Future for TrackedFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We only access inner through a pinned reference
        let this = unsafe { self.get_unchecked_mut() };

        // Acquire track ID on first poll
        if this.track_id.is_none() {
            this.track_id = Some(acquire_track_id());
        }

        let track_id = this.track_id.unwrap();

        // Save current track ID and set ours
        let previous_track_id = get_current_track_id();
        set_current_track_id(track_id);

        // Poll the inner future
        // SAFETY: inner is structurally pinned
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        let result = inner.poll(cx);

        // Restore previous track ID
        set_current_track_id(previous_track_id);

        // If complete, release the track ID
        if result.is_ready() {
            release_track_id(track_id);
            this.track_id = None;
        }

        result
    }
}

/// Wrap a future to be executed on its own Perfetto track.
///
/// This function assigns a unique track ID to the future, making it appear
/// on a separate track in the Perfetto UI. This is useful for visualizing
/// concurrent async tasks.
///
/// # Example
///
/// ```rust,ignore
/// use pluvio_runtime::track::tracked;
///
/// // Spawn a task with its own track
/// runtime.spawn(tracked(async {
///     // This task's spans will appear on a dedicated track
///     do_work().await
/// }));
/// ```
pub fn tracked<F: Future>(future: F) -> TrackedFuture<F> {
    TrackedFuture {
        inner: future,
        track_id: None,
    }
}

/// An enum wrapper that conditionally tracks a future.
///
/// This allows spawn functions to optionally wrap futures with track ID management
/// at runtime without requiring boxing.
pub enum MaybeTracked<F: Future> {
    /// Future with track ID management
    Tracked(TrackedFuture<F>),
    /// Future without track ID management
    Untracked(F),
}

impl<F: Future> Future for MaybeTracked<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We're projecting the pin to the inner future
        unsafe {
            match self.get_unchecked_mut() {
                MaybeTracked::Tracked(tf) => Pin::new_unchecked(tf).poll(cx),
                MaybeTracked::Untracked(f) => Pin::new_unchecked(f).poll(cx),
            }
        }
    }
}

/// Conditionally wrap a future with tracking based on the enable flag.
///
/// When `enable_tracking` is true, the future will be wrapped with a `TrackedFuture`
/// that assigns it a unique track ID. When false, the future is returned as-is
/// but wrapped in `MaybeTracked::Untracked` for type compatibility.
pub fn maybe_tracked<F: Future>(future: F, enable_tracking: bool) -> MaybeTracked<F> {
    if enable_tracking {
        MaybeTracked::Tracked(TrackedFuture {
            inner: future,
            track_id: None,
        })
    } else {
        MaybeTracked::Untracked(future)
    }
}
