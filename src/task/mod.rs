// use std::any::Any;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable};
use std::{future::Future, pin::Pin, task::Waker};
// use std::io::Result;
use crossbeam_channel::Sender;


// SharedState の定義
#[derive(Debug)]
pub struct SharedState<T> {
    pub waker: Mutex<Option<Waker>>,
    pub result: Mutex<Option<Result<T, String>>>,
}

impl<T> Default for SharedState<T> {
    fn default() -> Self {
        Self::new()
    }
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

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let shared = self.shared_state.lock().unwrap();

        // 既に結果がある場合は Ready を返す
        if let Some(result) = shared.result.lock().unwrap().take() {
            tracing::trace!("JoinHandle completed");
            return Poll::Ready(result);
        }

        // Waker を登録
        let waker = cx.waker().clone();
        let mut waker_slot = shared.waker.lock().unwrap();
        *waker_slot = Some(waker);

        Poll::Pending
    }
}

// Task 構造体の定義
pub struct Task<T: 'static> {
    pub future: Arc<Mutex<Pin<Box<dyn Future<Output = ()> + 'static>>>>,
    pub task_sender: Sender<Arc<dyn TaskTrait>>,
    pub shared_state: Arc<Mutex<SharedState<T>>>,
}

pub trait TaskTrait {
    fn poll_task(self: Arc<Self>) -> Poll<()>;
    fn schedule(self: Arc<Self>);
}

impl<T> TaskTrait for Task<T>
where
    T: 'static,
{
    fn poll_task(self: Arc<Self>) -> Poll<()> {
        // let waker = waker_fn::waker_fn({
        //     let task = self.clone();
        //     move || {
        //         tracing::trace!("TaskTrait::poll_task waker_fn");
        //         // タスクを再スケジュール
        //         task.task_sender
        //             .send(task.clone())
        //             .expect("Failed to send task");
        //     }
        // });
        let waker = new_waker(self.clone());

        let mut context = Context::from_waker(&waker);
        let mut future_slot = self.future.lock().unwrap();

        match future_slot.as_mut().poll(&mut context) {
            Poll::Pending => {
                Poll::Pending
            }
            Poll::Ready(t) => {
                Poll::Ready(t)
            }
        }
    }

    fn schedule(self: Arc<Self>) {
        self.task_sender
            .send(self.clone())
            .expect("Failed to send task");
    }
}

unsafe fn clone_raw<T>(data: *const ()) -> RawWaker
where
    T: 'static,
{
    let arc: Arc<Task<T>> = Arc::from_raw(data as *const Task<T>);
    let arc_clone = arc.clone();
    let ptr = Arc::into_raw(arc_clone) as *const ();
    // 元のArcの参照カウントを戻す
    let _ = Arc::into_raw(arc);
    RawWaker::new(ptr, get_vtable::<T>())
}

unsafe fn wake_raw<T>(data: *const ())
where
    T: 'static,
{
    let arc: Arc<Task<T>> = Arc::from_raw(data as *const Task<T>);
    arc.schedule();
    // arcはここでdropされる
}

unsafe fn wake_by_ref_raw<T>(data: *const ())
where
    T: 'static,
{
    let arc: Arc<Task<T>> = Arc::from_raw(data as *const Task<T>);
    Arc::clone(&arc).schedule();
    let _ = Arc::into_raw(arc);
}

unsafe fn drop_raw<T>(data: *const ())
where
    T: 'static,
{
    drop(Arc::<Task<T>>::from_raw(data as *const Task<T>));
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

fn new_waker<T>(task: Arc<Task<T>>) -> Waker
where
    T: 'static,
{
    let raw = RawWaker::new(Arc::into_raw(task) as *const (), get_vtable::<T>());
    unsafe { Waker::from_raw(raw) }
}
