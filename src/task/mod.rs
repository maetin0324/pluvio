pub mod stat;
mod waker;

use std::cell::RefCell;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
// use crossbeam_channel::Sender;
use std::any::Any;
use std::sync::mpsc::Sender;

use crate::task::stat::TaskStat;
use crate::task::waker::new_waker;

// SharedState の定義
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
    pub fn new() -> Self {
        SharedState {
            waker: RefCell::new(None),
            result: RefCell::new(None),
        }
    }

    pub fn new_with_wrapped() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self::new()))
    }
}

pub struct JoinHandle<T> {
    pub shared_state: Rc<RefCell<SharedState>>,
    pub type_data: std::marker::PhantomData<T>,
}

impl<T> Future for JoinHandle<T>
where
    T: 'static,
{
    type Output = Result<T, String>;

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

// Task 構造体の定義
pub struct Task {
    pub future: Rc<RefCell<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub task_sender: Sender<usize>,
    pub shared_state: Rc<RefCell<SharedState>>,
    pub(super) task_stat: Option<stat::TaskStat>,
}

impl Task {
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

pub trait TaskTrait {
    fn poll_task(self: &Self, task_id: usize) -> std::task::Poll<()>;
    // fn schedule(self: Rc<Self>);
}

impl TaskTrait for Task {
    fn poll_task(self: &Self, task_id: usize) -> std::task::Poll<()> {
        // let waker: Waker;
        // if let None = self.shared_state.borrow().waker.borrow().as_ref() {
        //     let weak = Rc::downgrade(&self);
        //     waker = new_waker(weak);
        // } else {
        //     waker = self.shared_state.borrow().waker.borrow().as_ref().unwrap().clone();
        // }
        let now = std::time::Instant::now();

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
        self.task_stat.as_ref().unwrap().add_execute_time(now.elapsed().as_nanos() as u64);

        ret
    }

    // fn schedule(self: Rc<Self>) {
    //     self.task_sender
    //         .send(self.id)
    //         .expect("Failed to send task");
    // }
}


