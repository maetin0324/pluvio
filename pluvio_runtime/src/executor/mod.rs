//! Task executor for the pluvio runtime.
//!
//! This module contains the [`Runtime`] which schedules tasks and polls
//! registered reactors. It also provides statistics utilities used to
//! inspect running tasks.

pub mod stat;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    rc::Rc,
    task::Poll,
};

use slab::Slab;
use std::sync::mpsc::{channel, Receiver, Sender};

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

impl Runtime {
    /// Creates a new runtime with an internal task queue of `queue_size`.
    pub fn new(queue_size: u64) -> Rc<Self> {
        // allocator.fill_buffers(0x61);
        let (task_sender, task_receiver) = channel();
        let (polling_task_sender, polling_task_receiver) = channel();
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
    /// its result.
    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
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
    pub fn spawn_polling<F, T>(&self, future: F) -> JoinHandle<T>
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
    pub fn spawn_with_name<F, T>(&self, future: F, task_name: String) -> JoinHandle<T>
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
    /// statistics purposes.
    pub fn spawn_polling_with_name<F, T>(&self, future: F, task_name: String) -> JoinHandle<T>
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
        // while !self.task_receiver.is_empty() || !self.reactor.completions.borrow_mut().is_empty()
        // {
        let mut noop_counter: u64 = 0;
        let mut _nooped = 0;
        while self.task_pool.borrow().len() > 0 {
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
                }
            }

            let task_id_slot = self.task_receiver.try_recv();

            if let Ok(task_id) = task_id_slot {
                // タスクを取得してポーリング
                tracing::trace!("Runtime::run_queue task_id: {}", task_id);
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    let mut binding = self.task_pool.borrow_mut();
                    let task = binding.get_mut(task_id);
                    self.stat.add_task_stat(task);

                    binding.remove(task_id);
                    tracing::trace!(
                        "Task {} completed, remaining tasks: {}",
                        task_id,
                        binding.len()
                    );
                }
            } else {
                tracing::trace!("No task to poll");
                noop_counter += 1;
                if noop_counter > 100 {
                    // tracing::trace!("No tasks for a while, sleeping...");

                    // if nooped > 100 {
                    //     tracing::debug!("No tasks for a while, breaking...");
                    //     break;
                    // }
                }
            }

            // Reactorの処理
            for (id, reactor_wrapper) in self.reactors.borrow().iter() {
                if reactor_wrapper.enable.get() {
                    tracing::trace!("Polling reactor: {}", id);
                    if let ReactorStatus::Running = reactor_wrapper.reactor.status() {
                        let now = std::time::Instant::now();
                        reactor_wrapper.reactor.poll();
                        self.stat
                            .add_pool_and_completion_time(now.elapsed().as_nanos() as u64);
                        reactor_wrapper
                            .poll_counter
                            .set(reactor_wrapper.poll_counter.get() + 1);
                        tracing::trace!("Reactor {} polled", id);
                    } else {
                        tracing::trace!("Reactor {} is not running", id);
                    }
                    // let now = std::time::Instant::now();
                    // reactor_wrapper.reactor.poll();
                    // self.stat
                    //     .add_pool_and_completion_time(now.elapsed().as_nanos() as u64);
                } else {
                    tracing::trace!("Reactor {} is disabled", id);
                }
            }

            // イベントループの待機（適宜調整）
            // std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Run the provided future to completion, driving the event loop.
    pub fn run<F, T>(&self, future: F)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        self.spawn(future);
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
