#![allow(dead_code)]
use std::cell::Cell;
use std::{cell::RefCell, rc::Rc};
use std::sync::atomic::{AtomicUsize, Ordering};

use slab::Slab;

use crate::worker::Worker;
use pluvio_runtime::reactor::ReactorStatus;

thread_local! {
    pub static PLUVIO_UCX_REACTOR: std::cell::OnceCell<Rc<UCXReactor>> = std::cell::OnceCell::new();
}

/// Per-reactor poll() interval statistics for UCXReactor.
///
/// Writes one CSV row per poll() call into a BufWriter so that data is flushed
/// incrementally and survives SIGKILL (lost data is bounded by the buffer size).
pub struct ReactorStats {
    last_ns: Option<u64>,
    start: std::time::Instant,
    count: usize,
    csv_writer: Option<std::io::BufWriter<std::fs::File>>,
}

impl ReactorStats {
    /// Create a new stats recorder. If `csv_path` is provided, opens the file
    /// for streaming writes.
    pub fn new(csv_path: Option<&str>) -> std::io::Result<Self> {
        use std::io::Write;
        let csv_writer = match csv_path {
            Some(path) => {
                let f = std::fs::File::create(path)?;
                let mut w = std::io::BufWriter::with_capacity(1 << 16, f);
                writeln!(w, "interval_us")?;
                Some(w)
            }
            None => None,
        };
        Ok(Self {
            last_ns: None,
            start: std::time::Instant::now(),
            count: 0,
            csv_writer,
        })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    /// Record a poll() call and append the interval to the CSV file.
    #[inline]
    pub fn record(&mut self) {
        use std::io::Write;
        let now_ns = self.start.elapsed().as_nanos() as u64;
        if let Some(prev) = self.last_ns {
            let delta_us = (now_ns - prev) / 1000;
            if let Some(w) = self.csv_writer.as_mut() {
                let _ = writeln!(w, "{}", delta_us);
            }
        }
        self.last_ns = Some(now_ns);
        self.count += 1;
    }

    /// Explicitly flush the underlying BufWriter. Call before normal exit.
    pub fn flush(&mut self) {
        use std::io::Write;
        if let Some(w) = self.csv_writer.as_mut() {
            let _ = w.flush();
        }
    }
}

pub struct UCXReactor {
    registered_workers: RefCell<Slab<Rc<Worker>>>,
    last_polled: RefCell<std::time::Instant>,
    connection_timeout: std::time::Duration,
    /// Atomic counter for active/waiting workers to avoid iteration in status()
    /// This is called very frequently (313K+ times in benchmarks)
    active_or_waiting_count: AtomicUsize,
    /// Optional poll() interval statistics. Enabled via `enable_stats()`.
    /// Thread-local (single-threaded executor), so RefCell + Cell suffice.
    stats: RefCell<Option<ReactorStats>>,
    /// Fast-path check to avoid borrowing `stats` on every poll() call.
    stats_enabled: Cell<bool>,
}

impl UCXReactor {
    #[tracing::instrument(level = "trace")]
    pub fn new() -> Self {
        // Read timeout from environment variable or use default of 30 seconds
        let timeout_secs = std::env::var("PLUVIO_CONNECTION_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(30);

        Self {
            registered_workers: RefCell::new(Slab::new()),
            last_polled: RefCell::new(std::time::Instant::now()),
            connection_timeout: std::time::Duration::from_secs(timeout_secs),
            active_or_waiting_count: AtomicUsize::new(0),
            stats: RefCell::new(None),
            stats_enabled: Cell::new(false),
        }
    }

    #[tracing::instrument(level = "trace")]
    pub fn current() -> Rc<Self> {
        PLUVIO_UCX_REACTOR.with(|cell| cell.get_or_init(|| Rc::new(Self::new())).clone())
    }

    #[tracing::instrument(level = "trace", skip(self, worker))]
    pub fn register_worker(&self, worker: Rc<Worker>) -> usize {
        self.registered_workers.borrow_mut().insert(worker)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    pub fn unregister_worker(&self, id: usize) {
        self.registered_workers.borrow_mut().remove(id);
    }

    /// Increment the active/waiting worker count.
    /// Called when a worker transitions to Active or WaitConnect state.
    pub fn increment_active_count(&self) {
        self.active_or_waiting_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the active/waiting worker count.
    /// Called when a worker transitions from Active or WaitConnect to Inactive.
    pub fn decrement_active_count(&self) {
        self.active_or_waiting_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Enable per-poll-call interval recording. If `csv_path` is provided,
    /// intervals are streamed to that file with BufWriter (periodic auto-flush).
    /// Call this BEFORE the runtime starts running tasks.
    pub fn enable_stats(&self, csv_path: Option<&str>) -> std::io::Result<()> {
        *self.stats.borrow_mut() = Some(ReactorStats::new(csv_path)?);
        self.stats_enabled.set(true);
        Ok(())
    }

    /// Disable interval recording.
    pub fn disable_stats(&self) {
        self.stats_enabled.set(false);
    }

    /// Flush the stats CSV writer explicitly.
    pub fn flush_stats(&self) {
        if let Some(s) = self.stats.borrow_mut().as_mut() {
            s.flush();
        }
    }

    /// Number of recorded poll() invocations (0 if stats disabled).
    pub fn stats_len(&self) -> usize {
        self.stats.borrow().as_ref().map(|s| s.len()).unwrap_or(0)
    }
}

impl pluvio_runtime::reactor::Reactor for UCXReactor {
    fn status(&self) -> ReactorStatus {
        // Use atomic counter to avoid RefCell borrow and worker iteration overhead.
        // This method is called very frequently (313K+ times in benchmarks),
        // so avoiding the RefCell borrow and iteration is critical for performance.
        if self.active_or_waiting_count.load(Ordering::Relaxed) > 0 {
            ReactorStatus::Running
        } else {
            ReactorStatus::Stopped
        }
    }

    fn poll(&self) {
        // Record poll() call BEFORE actual polling work, only when stats enabled.
        // Cell::get is a cheap branch; record() streams to BufWriter so data
        // survives SIGKILL up to the buffer size.
        if self.stats_enabled.get() {
            if let Ok(mut s) = self.stats.try_borrow_mut() {
                if let Some(stats) = s.as_mut() {
                    stats.record();
                }
            }
        }
        let workers = self.registered_workers.borrow();
        for (_, worker) in workers.iter() {
            worker.inner().progress();
        }
    }
}
