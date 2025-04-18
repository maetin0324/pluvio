use std::{
    cell::RefCell, future::Future, rc::Rc, time::Duration
};

use crossbeam_channel::{unbounded, Receiver, Sender};

use crate::{
    io::allocator::FixedBufferAllocator,
    reactor::Reactor,
    task::{JoinHandle, SharedState, Task, TaskTrait},
};

// Runtime の定義
pub struct Runtime {
    pub reactor: Rc<Reactor>,
    task_sender: Sender<Rc<dyn TaskTrait>>,
    polling_task_sender: Sender<Rc<dyn TaskTrait>>,
    pub task_receiver: Receiver<Rc<dyn TaskTrait>>,
    pub polling_task_receiver: Receiver<Rc<dyn TaskTrait>>,
    pub allocator: Rc<FixedBufferAllocator>,
}

impl Runtime {
    pub fn new(
        queue_size: u32,
        buffer_size: usize,
        submit_depth: u32,
        wait_timeout: u64,
    ) -> Rc<Self> {
        let reactor = Rc::new(Reactor::new(
            queue_size,
            submit_depth,
            Duration::from_millis(wait_timeout),
        ));
        let allocator = FixedBufferAllocator::new(
            queue_size as usize,
            buffer_size,
            &mut reactor.ring.borrow_mut(),
        );
        allocator.fill_buffers(0x61);
        let (task_sender, task_receiver) = unbounded();
        let (polling_task_sender, polling_task_receiver) = unbounded();
        Rc::new(Runtime {
            reactor,
            task_sender,
            polling_task_sender,
            task_receiver,
            polling_task_receiver,
            allocator,
        })
    }

    pub fn spawn<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let shared = Rc::new(RefCell::new(SharedState::<T> {
            result: RefCell::new(None),
            waker: RefCell::new(None),
        }));

        let handle = JoinHandle {
            shared_state: shared.clone(),
        };

        // Clone shared before moving into async block
        let shared_clone = shared.clone();
        let wrapped_future = async move {
            let res = future.await;
            {
                let binding = shared_clone.borrow_mut();
                let mut result = binding.result.borrow_mut();
                *result = Some(Ok(res));
            }
            if let Some(waker) = shared_clone.borrow_mut().waker.borrow_mut().take() {
                waker.wake();
            }
        };
        let task = Rc::new(Task {
            future: Rc::new(RefCell::new(Box::pin(wrapped_future))),
            task_sender: self.task_sender.clone(),
            shared_state: shared,
        });

        // タスクをキューに送信
        self.task_sender.send(task).expect("Failed to send task");

        handle
    }

    pub fn spawn_polling<F, T>(&self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let shared = Rc::new(RefCell::new(SharedState::<T> {
            result: RefCell::new(None),
            waker: RefCell::new(None),
        }));

        let handle = JoinHandle {
            shared_state: shared.clone(),
        };

        // Clone shared before moving into async block
        let shared_clone = shared.clone();
        let wrapped_future = async move {
            let res = future.await;
            {
                let binding = shared_clone.borrow_mut();
                let mut result = binding.result.borrow_mut();
                *result = Some(Ok(res));
            }
            if let Some(waker) = shared_clone.borrow_mut().waker.borrow_mut().take() {
                tracing::trace!("Runtime::spawn wake");
                waker.wake();
            }
        };
        tracing::trace!("Runtime::spawn wrapped_future");
        let task = Rc::new(Task {
            future: Rc::new(RefCell::new(Box::pin(wrapped_future))),
            task_sender: self.polling_task_sender.clone(),
            shared_state: shared,
        });

        // タスクをキューに送信
        self.polling_task_sender
            .send(task)
            .expect("Failed to send task");

        tracing::trace!("Runtime::spawn task_sent, return handle");
        handle
    }

    pub fn run_queue(&self) {
        // while !self.task_receiver.is_empty() || !self.reactor.completions.borrow_mut().is_empty()
        // {
        loop {
            // yield_nowされたタスクが入ると無限ループしてしまうので
            // 現時点でReceiverにあるタスクのみを処理
            // polling_task_receiverにあるタスクを一度に取り出して処理
            let mut polling_tasks = Vec::new();
            while let Ok(task) = self.polling_task_receiver.try_recv() {
                polling_tasks.push(task);
            }
            for task in polling_tasks {
                // tracing::debug!(
                //     "polling_task_receiver processing task, remaining tasks: {:?}",
                //     self.polling_task_receiver.len()
                // );
                task.poll_task();
            }

            if let Ok(task) = self.task_receiver.try_recv() {
                task.poll_task();
            }

            // Reactor の完了イベントをポーリング
            self.reactor.poll_submit_and_completions();

            // イベントループの待機（適宜調整）
            // std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    pub fn run<F, T>(&self, future: F)
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        self.spawn(future);
        self.run_queue();
    }

    pub fn register_file(&self, fd: i32) {
        self.reactor.register_file(fd);
    }

    // pub fn grow_buffers(&self) {
        
    // }
}
