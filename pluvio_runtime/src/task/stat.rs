//! Statistics tracking for individual tasks.

use std::cell::Cell;

#[derive(Clone)]
/// Execution statistics for a [`Task`](crate::task::Task).
pub struct TaskStat {
    pub task_name: Option<String>,
    pub execute_time_ns: Cell<u64>,
    pub start_time_ns: std::cell::OnceCell<std::time::Instant>,
    pub end_time_ns: std::cell::OnceCell<std::time::Instant>,
    pub running: Cell<bool>,
}

impl TaskStat {
    /// Create a new statistic record for a task.
    pub fn new(task_name: Option<String>) -> Self {
        TaskStat {
            task_name,
            execute_time_ns: Cell::new(0),
            start_time_ns: std::cell::OnceCell::new(),
            end_time_ns: std::cell::OnceCell::new(),
            running: Cell::new(true),
        }
    }

    /// Add execution time in nanoseconds to the total.
    pub fn add_execute_time(&self, time_ns: u64) {
        let current_time = self.execute_time_ns.get();
        self.execute_time_ns.set(current_time + time_ns);
    }

    /// Get the total execution time in nanoseconds.
    pub fn get_execute_time(&self) -> u64 {
        self.execute_time_ns.get()
    }

    /// Real time between the first poll and completion of the task.
    pub fn get_elapsed_real_time(&self) -> Option<std::time::Duration> {
        if let (Some(start), Some(end)) = (self.start_time_ns.get(), self.end_time_ns.get()) {
            Some(end.duration_since(*start))
        } else {
            None
        }
    }
}

impl std::fmt::Debug for TaskStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStatics")
            .field("task_name", &self.task_name)
            .field("execute_time_ns", &self.execute_time_ns.get())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn execute_time_accumulates() {
        let stat = TaskStat::new(Some("unit".into()));
        stat.add_execute_time(10);
        stat.add_execute_time(5);
        assert_eq!(stat.get_execute_time(), 15);
    }

    #[test]
    fn elapsed_real_time_returns_duration() {
        let stat = TaskStat::new(None);
        let start = Instant::now();
        let end = start + Duration::from_millis(5);
        stat.start_time_ns.set(start).unwrap();
        stat.end_time_ns.set(end).unwrap();

        assert_eq!(stat.get_elapsed_real_time(), Some(end - start));
    }
}
