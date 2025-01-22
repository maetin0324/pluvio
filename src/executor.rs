use std::{future::Future, sync::{Arc, Mutex}};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::{reactor::Reactor, Task};

// Runtime の定義
pub struct Runtime<T: 'static> {
    pub reactor: Arc<Reactor>,
    task_sender: Sender<Arc<Task<T>>>,
    task_receiver: Receiver<Arc<Task<T>>>,
}

impl<T> Runtime<T> {
    pub fn new(queue_size: u32) -> Self {
        let reactor = Arc::new(Reactor::new(queue_size));
        let (task_sender, task_receiver) = unbounded();
        Runtime {
            reactor,
            task_sender,
            task_receiver,
        }
    }

    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(future)),
            task_sender: self.task_sender.clone(),
        });

        // タスクをキューに送信
        self.task_sender.send(task).expect("Failed to send task");
    }

    /// ランタイムのイベントループを実行します。
    pub fn run_queue(&self) {
        loop {
            // タスクを処理
            while let Ok(task) = self.task_receiver.try_recv() {
                let _ = task.poll();
            }

            // Reactor の完了イベントをポーリング
            self.reactor.poll_completions();

            // イベントループの待機（適宜調整）
            std::thread::sleep(std::time::Duration::from_millis(10));

            if self.task_receiver.is_empty() && self.reactor.is_empty() {
                break;
            }

        }
    }

    pub fn run<F>(&self, future: F)
    where
        F: Future<Output = T> + Send + 'static,
    {
        self.spawn(future);
        self.run_queue();
    }
}