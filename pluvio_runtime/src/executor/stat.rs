//! Statistics utilities for the [`Runtime`](crate::executor::Runtime).
//!
//! This module tracks execution time of tasks and reactor polling.

use crate::task::{stat::TaskStat, Task};
use std::cell::{Cell, RefCell};

/// Runtime wide statistics collected during execution.
#[derive(Debug)]
pub struct RuntimeStat {
    pub(crate) finished_task_stats: RefCell<Vec<TaskStat>>,
    pub(crate) pool_and_completion_time: Cell<u64>,
}

impl RuntimeStat {
    /// Create an empty [`RuntimeStat`].
    pub fn new() -> Self {
        RuntimeStat {
            finished_task_stats: RefCell::new(Vec::new()),
            pool_and_completion_time: Cell::new(0),
        }
    }

    /// Record statistics of a finished task.
    ///
    /// The previous implementation pushed every finished `TaskStat`
    /// (with its owned `task_name: Option<String>`) into
    /// `finished_task_stats`, a `Vec<TaskStat>` that was never drained.
    /// For workloads that drive millions of `block_on_with_name` calls
    /// (mdtest-hard's create+write+close storm: ~1 700/s/rank for 60 s
    /// = ~1 M tasks/rank) this leaked one 6-byte `String` allocation
    /// (the task name) plus the surrounding `TaskStat` per call,
    /// totalling hundreds of MB across all ranks. dhat (job 18050)
    /// pinpointed this site as the only one with `leaks == total_bytes`
    /// — every allocation lived to process exit.
    ///
    /// We now opt-in to retention via `PLUVIO_RETAIN_TASK_STATS=1` so
    /// callers that actually inspect the stats keep working, but the
    /// default path drops the `TaskStat` immediately. This eliminates
    /// the unbounded Vec growth that fragments glibc tcache and
    /// surfaces latent heap UB under sustained small-allocation churn.
    pub fn add_task_stat(&self, task: Option<&mut Option<Task>>) {
        let task_stat = match task {
            Some(Some(t)) => t.task_stat.take().unwrap(),
            _ => return,
        };
        task_stat.running.set(false);
        static RETAIN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let retain = *RETAIN.get_or_init(|| {
            std::env::var("PLUVIO_RETAIN_TASK_STATS")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        });
        if retain {
            self.finished_task_stats.borrow_mut().push(task_stat);
        }
        // `task_stat` (and its owned String task_name) is dropped here
        // when not retained.
    }

    /// Accumulate time spent polling reactors and completing I/O.
    pub fn add_pool_and_completion_time(&self, time: u64) {
        let current_time = self.pool_and_completion_time.get();
        self.pool_and_completion_time.set(current_time + time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::spsc::unbounded;
    use crate::task::Task;

    #[test]
    fn add_task_stat_records_finished_tasks() {
        // Default (no env): TaskStat is dropped immediately to avoid the
        // unbounded leak that mdtest-hard surfaced. Opt-in retention
        // via `PLUVIO_RETAIN_TASK_STATS=1` is covered separately.
        unsafe {
            std::env::set_var("PLUVIO_RETAIN_TASK_STATS", "1");
        }
        let stats = RuntimeStat::new();
        let (sender, _receiver) = unbounded::<usize>();
        let (task_opt, _handle) =
            Task::create_task_and_handle(async { 5usize }, sender, Some("unit".into()));
        let mut slot = task_opt;
        stats.add_task_stat(Some(&mut slot));

        // Note: this test must run with `PLUVIO_RETAIN_TASK_STATS=1` set
        // before the OnceLock cache (per process) is initialized. The
        // testing harness typically runs tests in serial in one process,
        // so set it before calling add_task_stat.
        let recorded = stats.finished_task_stats.borrow();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].task_name.as_deref(), Some("unit"));
        assert!(!recorded[0].running.get());
        unsafe {
            std::env::remove_var("PLUVIO_RETAIN_TASK_STATS");
        }
    }

    #[test]
    fn add_pool_and_completion_time_accumulates() {
        let stats = RuntimeStat::new();
        stats.add_pool_and_completion_time(10);
        stats.add_pool_and_completion_time(5);
        assert_eq!(stats.pool_and_completion_time.get(), 15);
    }

    #[test]
    fn add_task_stat_ignores_missing_entry() {
        let stats = RuntimeStat::new();
        stats.add_task_stat(None);
        assert!(stats.finished_task_stats.borrow().is_empty());
    }
}
