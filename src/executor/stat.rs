use std::cell::{Cell, RefCell};
use crate::task::{stat::TaskStat, Task};

#[derive(Debug)]
pub struct RuntimeStat {
    pub(crate) finished_task_stats: RefCell<Vec<TaskStat>>,
    pub(crate) pool_and_completion_time: Cell<u64>,
}

impl RuntimeStat {
    pub fn new() -> Self {
        RuntimeStat {
            finished_task_stats: RefCell::new(Vec::new()),
            pool_and_completion_time: Cell::new(0),
        }
    }

    pub fn add_task_stat(&self, task: Option<&mut Option<Task>>) {
        let task_stat = match task {
            Some(Some(t)) => {
                t.task_stat.take().unwrap()
            },
            _ => return,
        };
        task_stat.running.set(false);
        self.finished_task_stats.borrow_mut().push(task_stat);
    }

    pub fn add_pool_and_completion_time(&self, time: u64) {
        let current_time = self.pool_and_completion_time.get();
        self.pool_and_completion_time.set(current_time + time);
    }
}