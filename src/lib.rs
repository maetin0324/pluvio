// use std::any::Any;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
use crossbeam_channel::Sender;

pub mod executor;
pub mod future;
pub mod reactor;

// SharedState の定義
#[derive(Debug)]
pub struct SharedState<T> {
    pub waker: Mutex<Option<Waker>>,
    pub result: Mutex<Option<Result<T, String>>>,
}

impl<T> SharedState<T> {
    pub fn new() -> Self {
        SharedState {
            waker: Mutex::new(None),
            result: Mutex::new(None),
        }
    }
}

pub struct JoinHandle<T> {
    pub shared_state: Arc<Mutex<SharedState<T>>>,
}

// Task 構造体の定義
pub struct Task<T: Send + Sync + 'static> {
    pub future: Arc<Mutex<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>>,
    pub task_sender: Sender<Arc<dyn TaskTrait>>,
    pub shared_state: Arc<Mutex<SharedState<T>>>,
}

pub trait TaskTrait: Send + Sync {
    fn poll_task(self: Arc<Self>) -> Poll<()>;
}

impl<T> TaskTrait for Task<T> where T: Send + Sync + 'static {
    fn poll_task(self: Arc<Self>) -> Poll<()> {
        let waker = waker_fn::waker_fn({
            let task = self.clone();
            move || {
                // タスクを再スケジュール
                task.task_sender
                    .send(task.clone())
                    .expect("Failed to send task");
            }
        });

        let mut context = Context::from_waker(&waker);
        let mut future_slot = self.future.lock().unwrap();

        match future_slot.as_mut().poll(&mut context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(t) => Poll::Ready(t),
        }
    }
}

impl<T> Task<T>
where
    T: Send + Sync + 'static,
{
    pub fn poll(self: Arc<Self>) -> Poll<()> {
        let waker = waker_fn::waker_fn({
            let task = self.clone();
            move || {
                // タスクを再スケジュール
                task.task_sender
                    .send(task.clone())
                    .expect("Failed to send task");
            }
        });

        let mut context = Context::from_waker(&waker);
        let mut future_slot = self.future.lock().unwrap();

        match future_slot.as_mut().poll(&mut context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(t) => Poll::Ready(t),
        }
    }
}
