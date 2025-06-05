use std::cell::RefCell;
use crate::task::{stat::TaskStat, Task};

#[derive(Debug)]
pub struct RuntimeStat {
    pub(crate) finished_task_stats: RefCell<Vec<TaskStat>>,
}

impl RuntimeStat {
    pub fn new() -> Self {
        RuntimeStat {
            finished_task_stats: RefCell::new(Vec::new()),
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
}