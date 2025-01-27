use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::{reactor::Reactor, JoinHandle, SharedState, Task, TaskTrait};

// Runtime の定義
pub struct Runtime {
    pub reactor: Arc<Reactor>,
    task_sender: Sender<Arc<dyn TaskTrait>>,
    pub task_receiver: Receiver<Arc<dyn TaskTrait>>,
}

impl Runtime {
    pub fn new(queue_size: u32) -> Arc<Self> {
        let reactor = Arc::new(Reactor::new(queue_size));
        let (task_sender, task_receiver) = unbounded();
        Arc::new(Runtime {
            reactor,
            task_sender,
            task_receiver,
        })
    }

    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + Send + Sync + 'static,
        T: Send + Sync + 'static,
    {
        let shared = Arc::new(Mutex::new(SharedState::<T> {
            result: Mutex::new(None),
            waker: Mutex::new(None),
        }));

        let handle = JoinHandle {
            shared_state: shared.clone(),
        };

        // Clone shared before moving into async block
        let shared_clone = shared.clone();
        let wrapped_future = async move {
            let res = future.await;
            {
                let binding = shared_clone.lock().unwrap();
                let mut result = binding.result.lock().unwrap();
                *result = Some(Ok(res));
            }
            if let Some(waker) = shared_clone.lock().unwrap().waker.lock().unwrap().take() {
                tracing::trace!("Runtime::spawn wake");
                waker.wake();
            }
        };
        tracing::trace!("Runtime::spawn wrapped_future");
        let task = Arc::new(Task {
            future: Arc::new(Mutex::new(Box::pin(wrapped_future))),
            task_sender: self.task_sender.clone(),
            shared_state: shared,
        });

        // タスクをキューに送信
        self.task_sender.send(task).expect("Failed to send task");

        tracing::trace!("Runtime::spawn task_sent, return handle");
        handle
    }

    /// ランタイムのイベントループを実行します。
    pub fn run_queue(&self) {
        while self.task_receiver.len() > 0 {
            tracing::trace!("event loop task_receiver: {:?}", self.task_receiver.len());
            // タスクを処理x
            while let Ok(task) = self.task_receiver.try_recv() {
                tracing::trace!("task_receiver: {:?}", self.task_receiver.len());
                let _ = task.poll_task();
            }

            // Reactor の完了イベントをポーリング
            self.reactor.poll_completions();

            // イベントループの待機（適宜調整）
            // std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    pub fn run<F, T>(&self, future: F)
    where
        F: Future<Output = T> + Send + Sync + 'static,
        T: Send + Sync + 'static,
    {
        self.spawn(future);
        self.run_queue();
    }
}
