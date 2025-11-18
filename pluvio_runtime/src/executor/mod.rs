//! Task executor for the pluvio runtime.
//!
//! This module contains the [`Runtime`] which schedules tasks and polls
//! registered reactors. It also provides statistics utilities used to
//! inspect running tasks.

pub mod spsc;
pub mod stat;

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashMap,
    future::Future,
    rc::Rc,
    task::Poll,
};

use slab::Slab;
// use std::sync::mpsc::{channel, Receiver, Sender};
use crate::executor::spsc::{unbounded, Receiver, Sender};

use crate::{
    executor::stat::RuntimeStat,
    reactor::{Reactor, ReactorStatus},
    task::{JoinHandle, Task, TaskTrait},
};

/// Internal wrapper used to keep state for a registered reactor.
struct ReactorWrapper<R> {
    /// The reactor instance.
    reactor: R,
    /// Number of times the reactor has been polled.
    poll_counter: Cell<usize>,
    /// Whether this reactor is currently enabled.
    enable: Cell<bool>,
}

/// Asynchronous task runtime that manages reactors and tasks.
pub struct Runtime {
    reactors: RefCell<HashMap<&'static str, ReactorWrapper<Rc<dyn Reactor>>>>,
    task_sender: Sender<usize>,
    polling_task_sender: Sender<usize>,
    pub task_receiver: Receiver<usize>,
    pub polling_task_receiver: Receiver<usize>,
    pub task_pool: Rc<RefCell<Slab<Option<Task>>>>,
    stat: RuntimeStat,
}

// Thread-local storage for the runtime
thread_local! {
    pub static RUNTIME: OnceCell<Rc<Runtime>> = OnceCell::new();
}


impl Runtime {
    /// Creates a new runtime with an internal task queue of `queue_size`.
    pub fn new(queue_size: u64) -> Rc<Self> {
        // allocator.fill_buffers(0x61);
        let (task_sender, task_receiver) = unbounded();
        let (polling_task_sender, polling_task_receiver) = unbounded();
        let task_pool = Rc::new(RefCell::new(Slab::with_capacity(queue_size as usize)));
        Rc::new(Runtime {
            reactors: RefCell::new(HashMap::new()),
            task_sender,
            polling_task_sender,
            task_receiver,
            polling_task_receiver,
            task_pool,
            stat: RuntimeStat::new(),
        })
    }

    pub fn set_affinity(&self, cpu_id: usize) {
        #[cfg(target_os = "linux")]
        {
            core_affinity::get_core_ids()
                .and_then(|core_ids| core_ids.into_iter().find(|c| c.id == cpu_id))
                .map(|core| {
                    core_affinity::set_for_current(core);
                    tracing::info!("Set runtime affinity to CPU {}", cpu_id);
                });
        }
    }

    /// Spawn a future onto the runtime and return a [`JoinHandle`] to await
    /// its result. This version explicitly takes a runtime reference.
    pub fn spawn_with_runtime<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (task, handle) = Task::create_task_and_handle(future, self.task_sender.clone(), None);

        // タスクをスレッドプールに追加
        let mut task_pool = self.task_pool.borrow_mut();
        let task_id = task_pool.insert(task);
        tracing::trace!("Runtime::spawn task_id: {}", task_id);

        // タスクをキューに送信
        self.task_sender.send(task_id).expect("Failed to send task");

        handle
    }

    /// Spawn a task that will be polled in a dedicated polling queue.
    /// This version explicitly takes a runtime reference.
    pub fn spawn_polling_with_runtime<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (task, handle) =
            Task::create_task_and_handle(future, self.polling_task_sender.clone(), None);

        // タスクをスレッドプールに追加
        let mut task_pool = self.task_pool.borrow_mut();
        let task_id = task_pool.insert(task);

        // タスクをキューに送信
        self.polling_task_sender
            .send(task_id)
            .expect("Failed to send task");

        tracing::trace!("Runtime::spawn task_sent, return handle");
        handle
    }

    /// Spawn a task and associate a name with it for statistics.
    /// This version explicitly takes a runtime reference.
    pub fn spawn_with_name_and_runtime<F, T>(&self, future: F, task_name: String) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (task, handle) =
            Task::create_task_and_handle(future, self.task_sender.clone(), Some(task_name));

        // タスクをスレッドプールに追加
        let mut task_pool = self.task_pool.borrow_mut();
        let task_id = task_pool.insert(task);
        tracing::trace!("Runtime::spawn_with_name task_id: {}", task_id);

        // タスクをキューに送信
        self.task_sender.send(task_id).expect("Failed to send task");

        handle
    }

    /// Spawn a task to the polling queue with a specific name for
    /// statistics purposes. This version explicitly takes a runtime reference.
    pub fn spawn_polling_with_name_and_runtime<F, T>(&self, future: F, task_name: String) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let (task, handle) =
            Task::create_task_and_handle(future, self.polling_task_sender.clone(), Some(task_name));

        // タスクをスレッドプールに追加
        let mut task_pool = self.task_pool.borrow_mut();
        let task_id = task_pool.insert(task);

        // タスクをキューに送信
        self.polling_task_sender
            .send(task_id)
            .expect("Failed to send task");

        tracing::trace!("Runtime::spawn_polling_with_name task_sent, return handle");
        handle
    }

    /// Register a reactor instance with this runtime.
    pub fn register_reactor<R>(&self, id: &'static str, reactor: R)
    where
        R: Reactor + 'static,
    {
        let reactor_wrapper = ReactorWrapper {
            reactor: Rc::new(reactor) as Rc<dyn Reactor>,
            poll_counter: Cell::new(0),
            enable: Cell::new(true),
        };
        self.reactors.borrow_mut().insert(id, reactor_wrapper);
        tracing::debug!("Reactor {} registered", id);
    }

    /// Run tasks until the task pool becomes empty.
    pub fn run_queue(&self) {
        let mut _nooped = 0;
        let mut stuck_counter: u64 = 0;

        // Read max stuck iterations from environment or use default
        let max_stuck_iterations = std::env::var("PLUVIO_MAX_STUCK_ITERATIONS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1000000);

        let mut sleep_duration = 1;

        while self.task_pool.borrow().len() > 0 {
            let mut made_progress = false;

            // IMPORTANT: Poll reactors FIRST to receive messages and wake tasks
            // This ensures that when a message arrives, tasks are woken immediately
            // and can be processed in the same iteration
            for (_id, reactor_wrapper) in self.reactors.borrow().iter() {
                if reactor_wrapper.enable.get() {
                    if let ReactorStatus::Running = reactor_wrapper.reactor.status() {
                        reactor_wrapper.reactor.poll();
                        reactor_wrapper
                            .poll_counter
                            .set(reactor_wrapper.poll_counter.get() + 1);
                        made_progress = true;
                    }
                }
            }

            // yield_nowされたタスクが入ると無限ループしてしまうので
            // 現時点でReceiverにあるタスクのみを処理
            // polling_task_receiverにあるタスクを一度に取り出して処理
            let mut polling_tasks = Vec::new();
            while let Ok(task) = self.polling_task_receiver.try_recv() {
                polling_tasks.push(task);
            }
            for task_id in polling_tasks {
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    // タスクが完了した場合、タスクを削除
                    let mut binding = self.task_pool.borrow_mut();
                    let task = binding.get_mut(task_id);
                    self.stat.add_task_stat(task);
                    binding.remove(task_id);
                    made_progress = true;
                }
            }

            let task_id_slot = self.task_receiver.try_recv();

            if let Ok(task_id) = task_id_slot {
                // タスクを取得してポーリング
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    let mut binding = self.task_pool.borrow_mut();
                    let task = binding.get_mut(task_id);
                    self.stat.add_task_stat(task);

                    binding.remove(task_id);
                    made_progress = true;
                }
            }

            // Check if runtime is stuck
            if !made_progress {
                stuck_counter += 1;
                if stuck_counter > max_stuck_iterations {
                    tracing::error!(
                        "Runtime stuck - no reactor making progress after {} iterations. Breaking out to prevent infinite loop.",
                        stuck_counter
                    );
                    self.log_running_task_stat();
                    break;
                }
                // Log periodically to help debugging
                if stuck_counter % 10000 == 0 {
                    tracing::debug!(
                        "Runtime may be stuck - no progress for {} iterations",
                        stuck_counter
                    );
                    if sleep_duration < (1 << 10) {
                        sleep_duration *= 2;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(sleep_duration));
                }
            } else {
                stuck_counter = 0;
                sleep_duration = 1;
            }

            // イベントループの待機（適宜調整）
            // std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Block on a specific future until it completes, without waiting for other tasks.
    ///
    /// This spawns the future as a task and runs the event loop until that specific
    /// task completes. Other background tasks continue running but don't block this call.
    ///
    /// # Arguments
    ///
    /// * `future` - The async function to execute
    ///
    /// # Returns
    ///
    /// The output of the future once it completes
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = runtime.block_on_with_runtime(async {
    ///     some_async_operation().await
    /// });
    /// ```
    pub fn block_on_with_runtime<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        self.block_on_with_name_and_runtime("unnamed_block_on", future)
    }

    /// Block on a specific future with a name until it completes.
    ///
    /// This is similar to `block_on_with_runtime` but includes a task name for debugging.
    ///
    /// # Arguments
    ///
    /// * `task_name` - Name for the task (for logging/debugging)
    /// * `future` - The async function to execute
    ///
    /// # Returns
    ///
    /// The output of the future once it completes
    pub fn block_on_with_name_and_runtime<F, T, S>(&self, task_name: S, future: F) -> T
    where
        F: Future<Output = T> + 'static,
        T: 'static,
        S: Into<String>,
    {
        // Wrap the future to capture its result
        let result_holder = Rc::new(RefCell::new(None));
        let result_holder_clone = result_holder.clone();

        let wrapped_future = async move {
            let result = future.await;
            *result_holder_clone.borrow_mut() = Some(result);
        };

        // Spawn the task and get its task_id
        let (task, _handle) = Task::create_task_and_handle(
            wrapped_future,
            self.task_sender.clone(),
            Some(task_name.into()),
        );

        let mut task_pool = self.task_pool.borrow_mut();
        let target_task_id = task_pool.insert(task);
        drop(task_pool);

        // Send the task to the queue
        self.task_sender
            .send(target_task_id)
            .expect("Failed to send task");

        // Run the event loop until OUR specific task completes
        let mut stuck_counter: u64 = 0;
        let max_stuck_iterations = std::env::var("PLUVIO_MAX_STUCK_ITERATIONS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1000000);

        loop {
            // Check if our target task has completed
            {
                let task_pool = self.task_pool.borrow();
                if !task_pool.contains(target_task_id) {
                    // Task completed and was removed from pool
                    break;
                }
            }

            let mut made_progress = false;

            // Poll reactors to drive I/O
            for (_id, reactor_wrapper) in self.reactors.borrow().iter() {
                if reactor_wrapper.enable.get() {
                    if let ReactorStatus::Running = reactor_wrapper.reactor.status() {
                        reactor_wrapper.reactor.poll();
                        reactor_wrapper
                            .poll_counter
                            .set(reactor_wrapper.poll_counter.get() + 1);
                        made_progress = true;
                    }
                }
            }

            // Process polling queue tasks
            let mut polling_tasks = Vec::new();
            while let Ok(task_id) = self.polling_task_receiver.try_recv() {
                polling_tasks.push(task_id);
            }
            for task_id in polling_tasks {
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    let mut binding = self.task_pool.borrow_mut();
                    let task = binding.get_mut(task_id);
                    self.stat.add_task_stat(task);
                    binding.remove(task_id);
                    made_progress = true;
                }
            }

            // Process regular queue tasks
            if let Ok(task_id) = self.task_receiver.try_recv() {
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    let mut binding = self.task_pool.borrow_mut();
                    let task = binding.get_mut(task_id);
                    self.stat.add_task_stat(task);
                    binding.remove(task_id);
                    made_progress = true;
                }
            }

            // Check for stuck condition
            if !made_progress {
                stuck_counter += 1;
                if stuck_counter > max_stuck_iterations {
                    tracing::error!(
                        "block_on stuck - no progress after {} iterations",
                        stuck_counter
                    );
                    self.log_running_task_stat();
                    panic!("block_on deadlock detected");
                }
                // Small yield to avoid busy loop
                std::thread::yield_now();
            } else {
                stuck_counter = 0;
            }
        }

        // Extract and return the result
        let result = result_holder.borrow_mut().take();
        result.expect("Task completed but result was not set")
    }

    /// Run the provided future to completion, driving the event loop.
    /// This version explicitly takes a runtime reference.
    pub fn run_with_runtime<F, T>(&self, future: F)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        self.run_with_name_and_runtime("unnamed_run", future);
    }

    /// Run the provided future with an explicit task name for easier tracing.
    /// This version explicitly takes a runtime reference.
    ///
    /// NOTE: This runs ALL tasks in the queue until completion. For blocking on
    /// a single task without waiting for background tasks, use `block_on_with_runtime`.
    pub fn run_with_name_and_runtime<F, T, S>(&self, task_name: S, future: F)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
        S: Into<String>,
    {
        self.spawn_with_name_and_runtime(future, task_name.into());
        self.run_queue();
    }

    /// Poll a single task by its id.
    pub fn poll_task(&self, task_id: usize) -> Poll<()> {
        let mut binding = self.task_pool.borrow_mut();
        let task_slot = binding.get_mut(task_id).expect("Task not found");
        let task = task_slot.take().expect("Task not found");
        drop(binding);
        let ret = task.poll_task(task_id);
        // taskを再度task_slotに格納
        let mut binding = self.task_pool.borrow_mut();
        let task_slot = binding.get_mut(task_id).expect("Task not found");
        task_slot.replace(task);
        ret
    }

    /// Progress the runtime by polling reactors and processing a limited number of tasks.
    /// This is useful for incremental runtime progress in a loop without blocking.
    pub fn progress(&self) {
        // Poll all enabled reactors to drive I/O progress
        for (_id, reactor_wrapper) in self.reactors.borrow().iter() {
            if reactor_wrapper.enable.get() {
                if let ReactorStatus::Running = reactor_wrapper.reactor.status() {
                    reactor_wrapper.reactor.poll();
                    reactor_wrapper
                        .poll_counter
                        .set(reactor_wrapper.poll_counter.get() + 1);
                }
            }
        }

        // Process all currently available tasks from the polling queue
        let mut polling_tasks = Vec::new();
        while let Ok(task_id) = self.polling_task_receiver.try_recv() {
            polling_tasks.push(task_id);
        }
        for task_id in polling_tasks {
            if let Poll::Ready(_) = self.poll_task(task_id) {
                let mut binding = self.task_pool.borrow_mut();
                let task = binding.get_mut(task_id);
                self.stat.add_task_stat(task);
                binding.remove(task_id);
            }
        }

        // Process all currently available tasks from the regular queue
        // Limit to prevent infinite loop if tasks keep spawning new tasks
        let max_tasks_per_call = 100;
        let mut processed = 0;
        while processed < max_tasks_per_call {
            if let Ok(task_id) = self.task_receiver.try_recv() {
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    let mut binding = self.task_pool.borrow_mut();
                    let task = binding.get_mut(task_id);
                    self.stat.add_task_stat(task);
                    binding.remove(task_id);
                }
                processed += 1;
            } else {
                break;
            }
        }
    }

    pub fn log_running_task_stat(&self) {
        let binding = self.task_pool.borrow();
        let running_task_stats = binding
            .iter()
            .filter_map(|(_, task)| task.as_ref().and_then(|t| t.task_stat.as_ref()))
            .collect::<Vec<_>>();
        tracing::debug!("Running Task Stats: {:?}", running_task_stats);
    }

    /// Output runtime statistics to the log.
    pub fn log_stat(&self) {
        let binding = self.task_pool.borrow();
        let running_task_stats = binding
            .iter()
            .filter_map(|(_, task)| task.as_ref().and_then(|t| t.task_stat.as_ref()))
            .collect::<Vec<_>>();
        tracing::debug!("Running Task Stats: {:?}", running_task_stats);
        tracing::debug!("Runtime Stats: {:?}", self.stat);
    }

    /// Retrieve statistics for tasks that contain the specified name.
    pub fn get_stats_by_name(&self, name: &str) -> Vec<crate::task::stat::TaskStat> {
        let binding = self.task_pool.borrow();
        let running_stats = binding
            .iter()
            .filter_map(|(_, task)| {
                task.as_ref().and_then(|t| {
                    if let Some(stat) = &t.task_stat {
                        if stat.task_name.as_deref().unwrap_or("").contains(name) {
                            Some(stat.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
            })
            .collect::<Vec<crate::task::stat::TaskStat>>();

        let binding = self.stat.finished_task_stats.borrow();
        let finished_stats = binding
            .iter()
            .filter(|stat| stat.task_name.as_deref() == Some(name))
            .cloned()
            .collect::<Vec<crate::task::stat::TaskStat>>();
        let mut all_stats = running_stats;
        all_stats.extend(finished_stats);
        all_stats
    }

    /// Get the total execution time for tasks whose name matches `name`.
    pub fn get_total_time(&self, name: &str) -> u64 {
        let binding = self.stat.finished_task_stats.borrow();
        let total_time = binding
            .iter()
            .filter(|stat| stat.task_name.as_deref().unwrap_or("").contains(name))
            .map(|stat| stat.execute_time_ns.get())
            .sum();
        total_time
    }

    /// Total time spent polling reactors in nanoseconds.
    pub fn get_reactor_polling_time(&self) -> u64 {
        self.stat.pool_and_completion_time.get()
    }

    /// Return statistics of the task that took the longest real time.
    pub fn get_longest_running_task(&self) -> Option<crate::task::stat::TaskStat> {
        let binding = self.stat.finished_task_stats.borrow();
        let finished_stats = binding
            .iter()
            .filter(|stat| stat.running.get() == false)
            .cloned()
            .collect::<Vec<crate::task::stat::TaskStat>>();

        finished_stats.into_iter().max_by_key(|stat| {
            stat.get_elapsed_real_time()
                .map_or(0, |d| d.as_nanos() as u64)
        })
    }

    // pub fn grow_buffers(&self) {

    // }
}

// TLS-based convenience functions

/// Set the thread-local runtime that will be used by TLS-based APIs.
pub fn set_runtime(runtime: Rc<Runtime>) {
    RUNTIME.with(|r| {
        r.set(runtime).unwrap_or_else(|_| panic!("Failed to set runtime"));
    });
}

/// Get a clone of the thread-local runtime.
pub fn get_runtime() -> Option<Rc<Runtime>> {
    RUNTIME.with(|r| r.get().cloned())
}

/// Clear the thread-local runtime. 
pub fn clear_runtime() {
    // RUNTIME.with(|r| {
    //     r.clear();
    // });
}


/// Spawn a future onto the thread-local runtime and return a [`JoinHandle`].
///
/// # Panics
///
/// Panics if no runtime has been set via [`set_runtime`].
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    get_runtime()
        .expect("No runtime set in thread-local storage. Call set_runtime first.")
        .spawn_with_runtime(future)
}

/// Spawn a task that will be polled in a dedicated polling queue.
///
/// # Panics
///
/// Panics if no runtime has been set via [`set_runtime`].
pub fn spawn_polling<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    get_runtime()
        .expect("No runtime set in thread-local storage. Call set_runtime first.")
        .spawn_polling_with_runtime(future)
}

/// Spawn a task with a name for statistics.
///
/// # Panics
///
/// Panics if no runtime has been set via [`set_runtime`].
pub fn spawn_with_name<F, T>(future: F, task_name: String) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    get_runtime()
        .expect("No runtime set in thread-local storage. Call set_runtime first.")
        .spawn_with_name_and_runtime(future, task_name)
}

/// Spawn a task to the polling queue with a specific name.
///
/// # Panics
///
/// Panics if no runtime has been set via [`set_runtime`].
pub fn spawn_polling_with_name<F, T>(future: F, task_name: String) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    get_runtime()
        .expect("No runtime set in thread-local storage. Call set_runtime first.")
        .spawn_polling_with_name_and_runtime(future, task_name)
}

/// Run the provided future to completion on the thread-local runtime.
///
/// # Panics
///
/// Panics if no runtime has been set via [`set_runtime`].
pub fn run<F, T>(future: F)
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    get_runtime()
        .expect("No runtime set in thread-local storage. Call set_runtime first.")
        .run_with_runtime(future)
}

/// Run the provided future with an explicit task name on the thread-local runtime.
///
/// # Panics
///
/// Panics if no runtime has been set via [`set_runtime`].
pub fn run_with_name<F, T, S>(task_name: S, future: F)
where
    F: Future<Output = T> + 'static,
    T: 'static,
    S: Into<String>,
{
    get_runtime()
        .expect("No runtime set in thread-local storage. Call set_runtime first.")
        .run_with_name_and_runtime(task_name, future)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn run_stops_after_simple_spawned_task() {
        let runtime = Runtime::new(4);
        runtime.spawn_with_runtime(async {});

        let start = Instant::now();
        runtime.run_with_name_and_runtime("run_stops_after_simple_spawned_task", async {});

        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "Runtime::run took {:?} which exceeds the 500ms budget",
            elapsed
        );
    }

    #[test]
    fn tls_spawn_and_run() {
        let runtime = Runtime::new(4);
        set_runtime(runtime.clone());

        spawn(async {
            // Simple task
        });

        let start = Instant::now();
        run_with_name("tls_spawn_and_run", async {});

        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "TLS-based run took {:?} which exceeds the 500ms budget",
            elapsed
        );

        clear_runtime();
    }

    #[test]
    #[should_panic(expected = "No runtime set in thread-local storage")]
    fn tls_spawn_without_runtime_panics() {
        spawn(async {});
    }
}
