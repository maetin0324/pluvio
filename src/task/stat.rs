use crate::task::Task;

pub struct TaskStat {
    pub task_name: Option<String>,
    pub execute_time_ns: u64,
}

impl TaskStat {
    pub fn new(task_name: Option<String>, execute_time_ns: u64) -> Self {
        TaskStat {
            task_name,
            execute_time_ns,
        }
    }

    pub fn new_from_task(task: &Task) -> Self {
        TaskStat {
            task_name: task.task_name.clone(),
            execute_time_ns: task.execute_time_ns.get(),
        }
    }
}

impl std::fmt::Debug for TaskStat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStatics")
            .field("task_name", &self.task_name)
            .field("execute_time_ns", &self.execute_time_ns)
            .finish()
    }
}

