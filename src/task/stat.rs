use std::cell::Cell;


#[derive(Clone)]
pub struct TaskStat {
    pub task_name: Option<String>,
    pub execute_time_ns: Cell<u64>,
    pub running: Cell<bool>,
}

impl TaskStat {
    pub fn new(task_name: Option<String>) -> Self {
        TaskStat {
            task_name,
            execute_time_ns: Cell::new(0),
            running: Cell::new(true),
        }
    }

    pub fn add_execute_time(&self, time_ns: u64) {
        let current_time = self.execute_time_ns.get();
        self.execute_time_ns.set(current_time + time_ns);
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

