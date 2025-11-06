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
    pub fn add_task_stat(&self, task: Option<&mut Option<Task>>) {
        let task_stat = match task {
            Some(Some(t)) => t.task_stat.take().unwrap(),
            _ => return,
        };
        task_stat.running.set(false);
        self.finished_task_stats.borrow_mut().push(task_stat);
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
        let stats = RuntimeStat::new();
        let (sender, _receiver) = unbounded::<usize>();
        let (task_opt, _handle) =
            Task::create_task_and_handle(async { 5usize }, sender, Some("unit".into()));
        let mut slot = task_opt;
        stats.add_task_stat(Some(&mut slot));

        let recorded = stats.finished_task_stats.borrow();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].task_name.as_deref(), Some("unit"));
        assert!(!recorded[0].running.get());
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
