pub mod stat;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
// use crossbeam_channel::Sender;
use std::any::Any;
use std::sync::mpsc::Sender;

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
    execute_time_ns: Cell<u64>,
    task_name: Option<String>,
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
            execute_time_ns: Cell::new(0),
            task_name,
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
        let execute_time_ns = self.execute_time_ns.get();

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

        self.execute_time_ns
            .set(now.elapsed().as_nanos() as u64 + execute_time_ns);

        ret
    }

    // fn schedule(self: Rc<Self>) {
    //     self.task_sender
    //         .send(self.id)
    //         .expect("Failed to send task");
    // }
}

#[derive(Debug)]
struct PluvioWaker {
    task_id: usize,
    task_sender: Sender<usize>,
}

// impl PluvioWaker {
//     fn debug_top_level(&self) {
//         if self.task_id == 0 {
//             tracing::debug!("PluvioWaker: task_id: {} was waked", self.task_id);
//         }
//     }
// }

unsafe fn clone_raw(data: *const ()) -> RawWaker {
    let rc: Rc<PluvioWaker> = Rc::from_raw(data as *const PluvioWaker);
    let rc_clone = rc.clone();
    let ptr = Rc::into_raw(rc_clone) as *const ();
    // rcのdropを防ぐ
    std::mem::forget(rc);
    RawWaker::new(ptr, get_vtable())
}

unsafe fn wake_raw(data: *const ()) {
    let rc: Rc<PluvioWaker> = Rc::from_raw(data as *const PluvioWaker);
    rc.task_sender
        .send(rc.task_id)
        .expect("Failed to send task");
}

unsafe fn wake_by_ref_raw(data: *const ()) {
    let rc: Rc<PluvioWaker> = Rc::from_raw(data as *const PluvioWaker);
    let rc_clone = rc.clone();
    rc_clone
        .task_sender
        .send(rc_clone.task_id)
        .expect("Failed to send task");
    std::mem::forget(rc);
}

unsafe fn drop_raw(data: *const ()) {
    drop(Rc::from_raw(data as *const PluvioWaker));
}

fn get_vtable() -> &'static RawWakerVTable {
    &RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw)
}

fn new_waker(sender: Sender<usize>, id: usize) -> Waker {
    let pluvio_waker = PluvioWaker {
        task_id: id,
        task_sender: sender,
    };
    let pluvio_waker = Rc::new(pluvio_waker);
    let raw = RawWaker::new(Rc::into_raw(pluvio_waker) as *const (), get_vtable());
    unsafe { Waker::from_raw(raw) }
}
