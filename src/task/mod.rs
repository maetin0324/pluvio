use std::cell::RefCell;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
// use crossbeam_channel::Sender;
use std::sync::mpsc::Sender;
use std::any::Any;


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
}

impl Drop for Task {
    fn drop(&mut self) {
        // tracing::debug!("Task dropped");
    }
}

impl std::fmt::Debug for Task
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // let pluvio_waker = unsafe {
        //     Rc::from_raw(self.shared_state.borrow().waker.borrow().as_ref().unwrap().data() as *const PluvioWaker)
        // };
        let ret = f.debug_struct("Task")
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

impl TaskTrait for Task
{
    fn poll_task(self: &Self, task_id: usize) -> std::task::Poll<()> {
        // let waker: Waker;
        // if let None = self.shared_state.borrow().waker.borrow().as_ref() {
        //     let weak = Rc::downgrade(&self);
        //     waker = new_waker(weak);
        // } else {
        //     waker = self.shared_state.borrow().waker.borrow().as_ref().unwrap().clone();
        // }

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
        future_slot.as_mut().poll(&mut context)
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

unsafe fn clone_raw(data: *const ()) -> RawWaker
{
    let rc: Rc<PluvioWaker> = Rc::from_raw(data as *const PluvioWaker);
    let rc_clone = rc.clone();
    let ptr = Rc::into_raw(rc_clone) as *const ();
    // rcのdropを防ぐ
    std::mem::forget(rc);
    RawWaker::new(ptr, get_vtable())
}

unsafe fn wake_raw(data: *const ())
{
    let rc: Rc<PluvioWaker> = Rc::from_raw(data as *const PluvioWaker);
    rc.task_sender.send(rc.task_id).expect("Failed to send task");
}

unsafe fn wake_by_ref_raw(data: *const ())
{
    let rc: Rc<PluvioWaker> = Rc::from_raw(data as *const PluvioWaker);
    let rc_clone = rc.clone();
    rc_clone.task_sender.send(rc_clone.task_id).expect("Failed to send task");
    std::mem::forget(rc);
}

unsafe fn drop_raw(data: *const ())
{
    drop(Rc::from_raw(data as *const PluvioWaker));
}

fn get_vtable() -> &'static RawWakerVTable
{
    &RawWakerVTable::new(
        clone_raw,
        wake_raw,
        wake_by_ref_raw,
        drop_raw,
    )
}

fn new_waker(sender: Sender<usize>, id: usize) -> Waker
{
    let pluvio_waker = PluvioWaker {
        task_id: id,
        task_sender: sender,
    };
    let pluvio_waker = Rc::new(pluvio_waker);
    let raw = RawWaker::new(Rc::into_raw(pluvio_waker) as *const (), get_vtable());
    unsafe { Waker::from_raw(raw) }
}