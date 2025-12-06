//! Timer reactor for pluvio runtime
//!
//! This module provides a custom reactor implementation that handles timer events
//! for the pluvio async runtime. Since pluvio doesn't have built-in timer support,
//! this reactor fills that gap by managing time-based wakeups.
//!
//! # Design
//!
//! The timer reactor maintains a list of pending timers sorted by deadline.
//! On each poll(), it checks if any timers have expired and wakes their tasks.
//!
//! # Usage
//!
//! ```ignore
//! let timer_reactor = TimerReactor::new();
//! runtime.register_reactor(timer_reactor);
//! ```

use pluvio_runtime::reactor::{Reactor, ReactorStatus};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::Waker;
use std::time::{Duration, Instant};

thread_local! {
    /// Shared instance of the TimerReactor for the current thread
    pub static PLUVIO_TIMER_REACTOR: Rc<TimerReactor> = Rc::new(TimerReactor::new());
}

/// Unique identifier for a timer with its deadline
///
/// This type encapsulates both the deadline and a unique ID for each timer,
/// ensuring that timers can be properly ordered and identified even when
/// multiple timers share the same deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerHandle {
    deadline: Instant,
    id: u64,
}

impl TimerHandle {
    /// Create a new timer handle with the given deadline
    fn new(deadline: Instant) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        Self {
            deadline,
            id: COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Get the deadline for this timer
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Create a sentinel handle representing the maximum possible handle at a given instant
    /// Used for range queries to find all timers up to a certain time
    fn max_at(deadline: Instant) -> Self {
        Self {
            deadline,
            id: u64::MAX,
        }
    }
}

/// Timer reactor for pluvio runtime
///
/// This reactor provides timer functionality using Instant-based deadlines.
/// Timers are checked on each `poll()` call and expired timers wake their tasks.
///
/// # Thread Safety
///
/// This reactor uses `RefCell` for interior mutability and is designed
/// for single-threaded use with the pluvio runtime. It is NOT thread-safe.
/// For multi-threaded scenarios, use a different timer implementation.
///
/// # Timer Resolution
///
/// Timers are checked on each `poll()` call. The actual resolution depends
/// on how frequently the runtime calls `poll()`. Timers are not guaranteed
/// to fire at exactly their deadline, only that they won't fire before.
///
/// # Cancellation
///
/// Timers can be cancelled using the `TimerHandle` returned from registration.
/// If a `Delay` future is dropped before completion, its timer is automatically
/// cancelled via the `Drop` implementation.
///
/// # Example
///
/// ```ignore
/// let timer_reactor = TimerReactor::new_shared();
/// runtime.register_reactor(timer_reactor.clone());
///
/// // Use with Delay future
/// let delay = Delay::new(timer_reactor.clone(), Duration::from_secs(1));
/// delay.await;
/// ```
pub struct TimerReactor {
    /// Timers indexed by handle for ordered expiration checking
    /// Using BTreeMap ensures timers are sorted by deadline
    timers: RefCell<BTreeMap<TimerHandle, Waker>>,
    /// Atomic counter for timer count to avoid RefCell borrow in status()
    /// This is updated on timer registration/cancellation/expiration
    timer_count_atomic: AtomicUsize,
}

impl Default for TimerReactor {
    fn default() -> Self {
        Self {
            timers: RefCell::new(BTreeMap::new()),
            timer_count_atomic: AtomicUsize::new(0),
        }
    }
}

impl TimerReactor {
    /// Create a new timer reactor
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Self {
        Self::default()
    }

    #[tracing::instrument(level = "trace")]
    pub fn current() -> Rc<Self> {
        PLUVIO_TIMER_REACTOR.with(|r| r.clone())
    }

    /// Register a timer that will wake the given waker at the specified deadline
    ///
    /// Returns a timer handle that can be used to cancel the timer
    #[tracing::instrument(level = "trace", skip(self, waker))]
    pub fn register_timer(&self, deadline: Instant, waker: Waker) -> TimerHandle {
        let handle = TimerHandle::new(deadline);
        self.timers.borrow_mut().insert(handle, waker);
        self.timer_count_atomic.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            "TimerReactor: registered timer {:?} with deadline in {:?}",
            handle,
            deadline.saturating_duration_since(Instant::now())
        );
        handle
    }

    /// Cancel a timer by its handle
    ///
    /// Returns true if the timer was found and canceled, false otherwise
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn cancel_timer(&self, handle: TimerHandle) -> bool {
        let removed = self.timers.borrow_mut().remove(&handle).is_some();
        if removed {
            self.timer_count_atomic.fetch_sub(1, Ordering::Relaxed);
        }
        removed
    }

    /// Get the number of pending timers
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn timer_count(&self) -> usize {
        self.timers.borrow().len()
    }

    /// Wake all expired timers and return the count of timers woken
    fn wake_expired_timers(&self, now: Instant) -> usize {
        let mut timers = self.timers.borrow_mut();

        // Early exit if no timers at all
        if timers.is_empty() {
            return 0;
        }

        // Check if earliest timer has expired
        if let Some((first_handle, _)) = timers.first_key_value() {
            if first_handle.deadline() > now {
                return 0; // No expired timers
            }
        }

        // Collect expired timer handles
        let expired: Vec<_> = timers
            .range(..=TimerHandle::max_at(now))
            .map(|(handle, _)| *handle)
            .collect();

        let count = expired.len();

        // Wake and remove expired timers
        for handle in expired {
            if let Some(waker) = timers.remove(&handle) {
                self.timer_count_atomic.fetch_sub(1, Ordering::Relaxed);
                tracing::debug!("TimerReactor: waking timer {:?}", handle);
                waker.wake();
            }
        }

        count
    }
}

impl Reactor for TimerReactor {
    fn poll(&self) {
        let expired_count = self.wake_expired_timers(Instant::now());

        if expired_count > 0 {
            tracing::debug!("TimerReactor: woke {} expired timers", expired_count);
        }
    }

    fn status(&self) -> ReactorStatus {
        // Use atomic counter to avoid RefCell borrow overhead
        // This is called very frequently (313K+ times in benchmarks)
        if self.timer_count_atomic.load(Ordering::Relaxed) > 0 {
            ReactorStatus::Running
        } else {
            ReactorStatus::Stopped
        }
    }
}

/// A future that resolves after a specified duration
///
/// This is a replacement for futures_timer::Delay that works with
/// the pluvio runtime's TimerReactor.
pub struct Delay {
    deadline: Instant,
    timer_handle: Option<TimerHandle>,
    reactor: Rc<TimerReactor>,
}

impl Delay {
    /// Create a new delay that will complete after the given duration
    #[tracing::instrument(level = "trace")]
    pub fn new(duration: Duration) -> Self {
        Self::new_at(Instant::now() + duration)
    }

    /// Create a delay with an absolute deadline
    #[tracing::instrument(level = "trace")]
    pub fn new_at(deadline: Instant) -> Self {
        Self {
            deadline,
            timer_handle: None,
            reactor: TimerReactor::current(),
        }
    }

    /// Get the deadline for this delay
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Get the remaining duration until the deadline
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

impl std::future::Future for Delay {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let now = Instant::now();

        // Check if already expired
        if now >= self.deadline {
            tracing::trace!("Delay: already expired, returning Ready");
            return std::task::Poll::Ready(());
        }

        // Register timer if not already registered
        if self.timer_handle.is_none() {
            tracing::debug!(
                "Delay: registering timer with deadline in {:?}",
                self.deadline.saturating_duration_since(now)
            );
            let handle = self
                .reactor
                .register_timer(self.deadline, cx.waker().clone());
            self.timer_handle = Some(handle);
        } else {
            tracing::trace!("Delay: timer already registered, returning Pending");
        }

        std::task::Poll::Pending
    }
}

#[tracing::instrument(level = "trace")]
pub fn sleep(duration: Duration) -> Delay {
    Delay::new(duration)
}

impl Drop for Delay {
    fn drop(&mut self) {
        // Clean up the timer if it's still registered
        if let Some(handle) = self.timer_handle {
            self.reactor.cancel_timer(handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_timer_reactor_basic() {
        let reactor = TimerReactor::current();
        assert_eq!(reactor.timer_count(), 0);
        assert_eq!(reactor.status(), ReactorStatus::Stopped);

        // Create a dummy waker
        let waker = futures::task::noop_waker();
        let deadline = Instant::now() + Duration::from_millis(100);

        // Register a timer
        let handle = reactor.register_timer(deadline, waker);
        assert_eq!(reactor.timer_count(), 1);
        assert_eq!(reactor.status(), ReactorStatus::Running);

        // Cancel the timer
        assert!(reactor.cancel_timer(handle));
        assert_eq!(reactor.timer_count(), 0);
        assert_eq!(reactor.status(), ReactorStatus::Stopped);
    }

    #[test]
    fn test_timer_expiration() {
        let reactor = TimerReactor::current();
        let waker = futures::task::noop_waker();

        // Register a timer that's already expired
        let deadline = Instant::now() - Duration::from_millis(100);
        reactor.register_timer(deadline, waker);

        assert_eq!(reactor.timer_count(), 1);
        assert_eq!(reactor.status(), ReactorStatus::Running);

        // Poll should remove the expired timer
        reactor.poll();
        assert_eq!(reactor.timer_count(), 0);
        assert_eq!(reactor.status(), ReactorStatus::Stopped);
    }

    #[test]
    fn test_multiple_timers_ordered_expiration() {
        let reactor = TimerReactor::current();
        let now = Instant::now();

        // Register timers with different deadlines
        reactor.register_timer(
            now + Duration::from_millis(300),
            futures::task::noop_waker(),
        );
        reactor.register_timer(
            now + Duration::from_millis(100),
            futures::task::noop_waker(),
        );
        reactor.register_timer(
            now + Duration::from_millis(200),
            futures::task::noop_waker(),
        );

        assert_eq!(reactor.timer_count(), 3);

        // Simulate time passing to 150ms
        std::thread::sleep(Duration::from_millis(150));
        reactor.poll();

        // Only first timer (100ms) should have expired
        assert_eq!(reactor.timer_count(), 2);
    }

    #[test]
    fn test_timer_handle_ordering() {
        let now = Instant::now();
        let handle1 = TimerHandle::new(now + Duration::from_secs(1));
        let handle2 = TimerHandle::new(now + Duration::from_secs(2));
        let handle3 = TimerHandle::new(now + Duration::from_secs(1));

        // Handles with earlier deadlines should be smaller
        assert!(handle1 < handle2);
        // Handles with same deadline but different IDs should be different
        assert_ne!(handle1, handle3);
        // But should maintain ordering by ID
        assert!(handle1 < handle3 || handle3 < handle1);
    }

    #[test]
    fn test_delay_methods() {
        let duration = Duration::from_secs(5);
        let delay = Delay::new(duration);

        // Check deadline is approximately correct
        let expected_deadline = Instant::now() + duration;
        let actual_deadline = delay.deadline();
        assert!(actual_deadline >= expected_deadline - Duration::from_millis(10));
        assert!(actual_deadline <= expected_deadline + Duration::from_millis(10));

        // Check remaining time
        let remaining = delay.remaining();
        assert!(remaining <= duration);
        assert!(remaining >= duration - Duration::from_millis(10));
    }

    #[test]
    fn test_delay_new_at() {
        let deadline = Instant::now() + Duration::from_secs(10);
        let delay = Delay::new_at(deadline);

        assert_eq!(delay.deadline(), deadline);
    }

    #[test]
    fn test_cancel_nonexistent_timer() {
        let reactor = TimerReactor::current();
        let fake_handle = TimerHandle::new(Instant::now());

        // Canceling a non-existent timer should return false
        assert!(!reactor.cancel_timer(fake_handle));
    }

    #[test]
    fn test_default_trait() {
        let reactor = TimerReactor::default();
        assert_eq!(reactor.timer_count(), 0);
    }
}
