use std::{
    cell::RefCell, future::Future, rc::Rc, task::Poll, time::Duration
};

// use crossbeam_channel::{unbounded, Receiver, Sender};
use std::sync::mpsc::{channel, Receiver, Sender};
use slab::Slab;

use crate::{
    io::allocator::FixedBufferAllocator,
    reactor::Reactor,
    task::{JoinHandle, SharedState, Task, TaskTrait},
};

// Runtime の定義
pub struct Runtime {
    pub reactor: Rc<Reactor>,
    task_sender: Sender<usize>,
    polling_task_sender: Sender<usize>,
    pub task_receiver: Receiver<usize>,
    pub polling_task_receiver: Receiver<usize>,
    pub task_pool: Rc<RefCell<Slab<Option<Box<dyn TaskTrait>>>>>,
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
        let (task_sender, task_receiver) = channel();
        let (polling_task_sender, polling_task_receiver) = channel();
        let task_pool = Rc::new(RefCell::new(Slab::with_capacity(queue_size as usize)));
        Rc::new(Runtime {
            reactor,
            task_sender,
            polling_task_sender,
            task_receiver,
            polling_task_receiver,
            task_pool,
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
        let task = Some(Box::new(Task {
            future: Rc::new(RefCell::new(Box::pin(wrapped_future))),
            task_sender: self.task_sender.clone(),
            shared_state: shared,
        }) as Box<dyn TaskTrait>);

        // タスクをスレッドプールに追加
        let mut task_pool = self.task_pool.borrow_mut();
        let task_id = task_pool.insert(task);
        tracing::trace!("Runtime::spawn task_id: {}", task_id);

        // タスクをキューに送信
        self.task_sender
            .send(task_id)
            .expect("Failed to send task");

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
        let task = Some(Box::new(Task {
            future: Rc::new(RefCell::new(Box::pin(wrapped_future))),
            task_sender: self.polling_task_sender.clone(),
            shared_state: shared,
        }) as Box<dyn TaskTrait>);

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
            for task_id in polling_tasks {
                
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    // タスクが完了した場合、タスクを削除
                    self.task_pool.borrow_mut().remove(task_id);
                } 
                
            }

            let task_id_slot = self.task_receiver.try_recv();

            if let Ok(task_id) = task_id_slot {
                // タスクを取得してポーリング
                if let Poll::Ready(_) = self.poll_task(task_id) {
                    // タスクが完了した場合、タスクを削除
                    self.task_pool.borrow_mut().remove(task_id);
                }
            } else {
                tracing::trace!("No task to poll");
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

    pub fn register_file(&self, fd: i32) {
        self.reactor.register_file(fd);
    }

    // pub fn grow_buffers(&self) {
        
    // }
}
