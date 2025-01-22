use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, task::Waker};
use std::io::Result;
use crossbeam_channel::Sender;

pub mod executor;
pub mod future;
pub mod reactor;

// SharedState の定義
pub struct SharedState {
  pub waker: Option<Waker>,
  pub result: Option<Result<usize>>,
}

impl SharedState {
  pub fn new() -> Self {
      SharedState {
          waker: None,
          result: None,
      }
  }
}

// Task 構造体の定義
pub struct Task<T: 'static> {
  pub future: Mutex<Pin<Box<dyn Future<Output = T> + Send>>>,
  pub task_sender: Sender<Arc<Task<T>>>,
}

impl<T> Task<T> {
  pub fn poll(self: Arc<Self>) -> Poll<T> {
      let waker = waker_fn::waker_fn({
          let task = self.clone();
          move || {
              // タスクを再スケジュール
              task.task_sender.send(task.clone()).expect("Failed to send task");
          }
      });

      let mut context = Context::from_waker(&waker);
      let mut future_slot = self.future.lock().unwrap();

      match future_slot.as_mut().poll(&mut context) {
          Poll::Pending => {Poll::Pending}
          Poll::Ready(t) => {
              Poll::Ready(t)
          }
      }
  }
}