//! Global statistics collection control for pluvio_ucx
//!
//! This module provides a global flag to enable/disable detailed timing
//! statistics collection. When disabled, timing measurements are skipped
//! to avoid performance overhead during production benchmarks.

use std::sync::atomic::{AtomicBool, Ordering};

/// Global flag for enabling detailed statistics collection
static STATS_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable statistics collection globally
pub fn set_stats_enabled(enabled: bool) {
    STATS_ENABLED.store(enabled, Ordering::SeqCst);
}

/// Check if statistics collection is enabled
#[inline]
pub fn is_stats_enabled() -> bool {
    STATS_ENABLED.load(Ordering::Relaxed)
}
