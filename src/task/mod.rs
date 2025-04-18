use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, RawWaker, RawWakerVTable};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
use crossbeam_channel::Sender;


// SharedState の定義
#[derive(Debug)]
pub struct SharedState<T> {
    pub waker: RefCell<Option<Waker>>,
    pub result: RefCell<Option<Result<T, String>>>,
}

impl<T> Default for SharedState<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> SharedState<T> {
    pub fn new() -> Self {
        SharedState {
            waker: RefCell::new(None),
            result: RefCell::new(None),
        }
    }
}

pub struct JoinHandle<T> {
    pub shared_state: Rc<RefCell<SharedState<T>>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let shared = self.shared_state.borrow_mut();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = shared.result.borrow_mut().take() {
            tracing::trace!("JoinHandle completed");
            return Poll::Ready(result);
        }

        // Waker を登録
        let waker = cx.waker().clone();
        let mut waker_slot = shared.waker.borrow_mut();
        *waker_slot = Some(waker);

        Poll::Pending
    }
}

// Task 構造体の定義
pub struct Task<T: 'static> {
    pub future: Rc<RefCell<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub task_sender: Sender<Rc<dyn TaskTrait>>,
    pub shared_state: Rc<RefCell<SharedState<T>>>,
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        tracing::debug!("Task dropped");
        // ここでタスクがドロップされたことを処理する
        // 例えば、タスクをキャンセルするなど
    }
}

pub trait TaskTrait {
    fn poll_task(self: Rc<Self>);
    fn schedule(self: Rc<Self>);
}

impl<T> TaskTrait for Task<T>
where
    T: 'static,
{
    fn poll_task(self: Rc<Self>) {
        // let waker: Waker;
        // if let None = self.shared_state.borrow().waker.borrow().as_ref() {
        //     let weak = Rc::downgrade(&self);
        //     waker = new_waker(weak);
        // } else {
        //     waker = self.shared_state.borrow().waker.borrow().as_ref().unwrap().clone();
        // }

        let waker = new_waker(Rc::downgrade(&self));

        let mut context = Context::from_waker(&waker);
        let mut future_slot = match self.future.try_borrow_mut() {
            Ok(future) => future,
            Err(_) => {
                tracing::warn!("Failed to borrow future");
                self.clone().schedule();
                return;
            }
        };

        // let _ = future_slot.as_mut().poll(&mut context);
        if let Poll::Pending = future_slot.as_mut().poll(&mut context) {
            drop(future_slot);
            std::mem::forget(self)
        } 
    }

    fn schedule(self: Rc<Self>) {
        self.task_sender
            .send(self.clone())
            .expect("Failed to send task");
    }
}

unsafe fn clone_raw<T>(data: *const ()) -> RawWaker
where
    T: 'static,
{
    let rc: Weak<Task<T>> = Weak::from_raw(data as *const Task<T>);
    let rc_clone = rc.clone();
    let ptr = Weak::into_raw(rc_clone) as *const ();
    // rcのdropを防ぐ
    std::mem::forget(rc);
    RawWaker::new(ptr, get_vtable::<T>())
}

unsafe fn wake_raw<T>(data: *const ())
where
    T: 'static,
{
    let rc: Weak<Task<T>> = Weak::from_raw(data as *const Task<T>);
    let rc = rc.upgrade().expect("Failed to upgrade weak reference");
    rc.schedule();
}

unsafe fn wake_by_ref_raw<T>(data: *const ())
where
    T: 'static,
{
    let rc: Weak<Task<T>> = Weak::from_raw(data as *const Task<T>);
    Weak::clone(&rc)
        .upgrade()
        .expect("Failed to upgrade weak reference")
        .schedule();
    std::mem::forget(rc);
}

unsafe fn drop_raw<T>(data: *const ())
where
    T: 'static,
{
    drop(Weak::<Task<T>>::from_raw(data as *const Task<T>));
}

fn get_vtable<T>() -> &'static RawWakerVTable
where
    T: 'static,
{
    &RawWakerVTable::new(
        clone_raw::<T>,
        wake_raw::<T>,
        wake_by_ref_raw::<T>,
        drop_raw::<T>,
    )
}

fn new_waker<T>(task: Weak<Task<T>>) -> Waker
where
    T: 'static,
{
    let raw = RawWaker::new(Weak::into_raw(task) as *const (), get_vtable::<T>());
    unsafe { Waker::from_raw(raw) }
}
fn hoge() {
    std::thread::sleep(std::time::Duration::from_secs(1));
}