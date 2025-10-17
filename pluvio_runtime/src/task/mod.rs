//! Task abstractions used by the [`Runtime`](crate::executor::Runtime).
//!
//! This module provides the [`Task`] type, its associated statistics and the
//! [`JoinHandle`] returned when spawning tasks.

pub mod stat;
mod waker;

use std::cell::RefCell;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
// use crossbeam_channel::Sender;
use crate::executor::spsc::Sender;
use std::any::Any;

use crate::task::stat::TaskStat;
use crate::task::waker::new_waker;

// SharedState の定義
/// Shared state between a running task and its [`JoinHandle`].
#[derive(Debug)]
pub struct SharedState {
    pub waker: RefCell<Option<Waker>>,
    pub result: RefCell<Option<Result<Box<dyn Any + 'static>, String>>>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedState {
    /// Create a new empty [`SharedState`].
    pub fn new() -> Self {
        SharedState {
            waker: RefCell::new(None),
            result: RefCell::new(None),
        }
    }

    /// Wrap the state in an `Rc<RefCell<...>>` for sharing.
    pub fn new_with_wrapped() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::new()))
    }
}

/// Handle returned from [`Runtime::spawn`](crate::executor::Runtime::spawn) that
/// allows awaiting the output of a task.
pub struct JoinHandle<T> {
    pub shared_state: Rc<RefCell<SharedState>>,
    pub type_data: std::marker::PhantomData<T>,
}

impl<T> Future for JoinHandle<T>
where
    T: 'static,
{
    type Output = Result<T, String>;

    /// Poll the underlying task and return its result when ready.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let shared = self.shared_state.borrow_mut();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = shared.result.borrow_mut().take() {
            // tracing::trace!("JoinHandle completed");
            let ret = match result {
                Ok(data) => {
                    let data = data.downcast::<T>();
                    match data {
                        Ok(data) => Ok(*data),
                        Err(_) => Err("Failed to downcast".to_string()),
                    }
                }
                Err(err) => Err(err),
            };
            return Poll::Ready(ret);
        }

        // Waker を登録
        let waker = cx.waker().clone();
        let mut waker_slot = shared.waker.borrow_mut();
        *waker_slot = Some(waker);

        Poll::Pending
    }
}

/// Internal representation of a spawned task.
pub struct Task {
    pub future: Rc<RefCell<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub task_sender: Sender<usize>,
    pub shared_state: Rc<RefCell<SharedState>>,
    pub(super) task_stat: Option<stat::TaskStat>,
}

impl Task {
    /// Create a new task from a future.
    pub fn new(
        future: Rc<RefCell<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
        task_sender: Sender<usize>,
        shared_state: Rc<RefCell<SharedState>>,
        task_name: Option<String>,
    ) -> Self {
        Task {
            future,
            task_sender,
            shared_state,
            task_stat: Some(TaskStat::new(task_name)),
        }
    }

    /// Wrap a future into a [`Task`] and return it with a [`JoinHandle`].
    pub fn create_task_and_handle<F, T>(
        future: F,
        sender: Sender<usize>,
        task_name: Option<String>,
    ) -> (Option<Task>, JoinHandle<T>)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let shared = SharedState::new_with_wrapped();

        let handle: JoinHandle<T> = JoinHandle {
            shared_state: shared.clone(),
            type_data: std::marker::PhantomData,
        };

        // Clone shared before moving into async block
        let shared_clone = shared.clone();
        let wrapped_future = async move {
            let res = future.await;
            {
                let binding = shared_clone.borrow_mut();
                let mut result = binding.result.borrow_mut();
                let res_box = Box::new(res);
                let res_any = res_box as Box<dyn std::any::Any>;
                *result = Some(Ok(res_any));
            }
            if let Some(waker) = shared_clone.borrow_mut().waker.borrow_mut().take() {
                tracing::trace!("Runtime::spawn wake");
                waker.wake();
            }
        };
        tracing::trace!("Runtime::spawn wrapped_future");
        let task = Some(Task::new(
            Rc::new(RefCell::new(Box::pin(wrapped_future))),
            sender,
            shared,
            task_name,
        ));

        (task, handle)
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        // tracing::debug!("Task dropped");
    }
}

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // let pluvio_waker = unsafe {
        //     Rc::from_raw(self.shared_state.borrow().waker.borrow().as_ref().unwrap().data() as *const PluvioWaker)
        // };
        let ret = f
            .debug_struct("Task")
            // .field("future", &self.future)
            .field("task_sender", &self.task_sender)
            .field("shared_state", &self.shared_state)
            // .field("waker", &pluvio_waker)
            .finish();
        // std::mem::forget(pluvio_waker);
        ret
    }
}

/// Trait implemented by runnable tasks.
pub trait TaskTrait {
    /// Poll the task once and return its progress.
    fn poll_task(self: &Self, task_id: usize) -> std::task::Poll<()>;
    // fn schedule(self: Rc<Self>);
}

impl TaskTrait for Task {
    /// Poll the future contained in this task and reschedule if pending.
    fn poll_task(self: &Self, task_id: usize) -> std::task::Poll<()> {
        // let waker: Waker;
        // if let None = self.shared_state.borrow().waker.borrow().as_ref() {
        //     let weak = Rc::downgrade(&self);
        //     waker = new_waker(weak);
        // } else {
        //     waker = self.shared_state.borrow().waker.borrow().as_ref().unwrap().clone();
        // }
        let now = std::time::Instant::now();

        if let Some(task_stat) = self.task_stat.as_ref() {
            let _ = task_stat.start_time_ns.set(now);
        }

        let waker = new_waker(self.task_sender.clone(), task_id);

        let mut context = Context::from_waker(&waker);
        let mut future_slot = match self.future.try_borrow_mut() {
            Ok(future) => future,
            Err(_) => {
                tracing::warn!("Failed to borrow future");
                return Poll::Pending;
            }
        };

        // let _ = future_slot.as_mut().poll(&mut context);
        let ret = future_slot.as_mut().poll(&mut context);
        self.task_stat
            .as_ref()
            .unwrap()
            .add_execute_time(now.elapsed().as_nanos() as u64);

        if &ret == &Poll::Ready(()) {
            if let Some(task_stat) = self.task_stat.as_ref() {
                let _ = task_stat.end_time_ns.set(now);
            }
            // tracing::debug!("Task {} completed", task_id);
        } else {
            // tracing::debug!("Task {} is pending", task_id);
        }

        ret
    }

    // fn schedule(self: Rc<Self>) {
    //     self.task_sender
    //         .send(self.id)
    //         .expect("Failed to send task");
    // }
}
