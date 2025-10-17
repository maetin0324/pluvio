//! Custom waker used by [`Task`](crate::task::Task).

use crate::executor::spsc::Sender;
use std::{
    rc::Rc,
    task::{RawWaker, RawWakerVTable, Waker},
};

/// Internal waker data used to reschedule tasks on the runtime.
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

/// Create a [`Waker`] that requeues the task onto the runtime.
pub(crate) fn new_waker(sender: Sender<usize>, id: usize) -> Waker {
    let pluvio_waker = PluvioWaker {
        task_id: id,
        task_sender: sender,
    };
    let pluvio_waker = Rc::new(pluvio_waker);
    let raw = RawWaker::new(Rc::into_raw(pluvio_waker) as *const (), get_vtable());
    unsafe { Waker::from_raw(raw) }
}
