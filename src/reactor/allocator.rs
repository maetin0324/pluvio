use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    mem::ManuallyDrop,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use io_uring::IoUring;
use libc::iovec;

pub const ALIGN: usize = 4096;
#[repr(align(4096))]
pub struct AlignedBuffer {
    pub data: [u8; 4096],
}

impl Default for AlignedBuffer {
    fn default() -> Self {
        AlignedBuffer { data: [0; 4096] }
    }
}

pub struct LazyAcquire {
    state: Rc<RefCell<LazyAcquireState>>,
    allocator: Rc<FixedBufferAllocator>,
}

impl Future for LazyAcquire {
    type Output = FixedBuffer;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(buffer) = this.allocator.acquire_inner() {
            Poll::Ready(buffer)
        } else {
            // If no buffer is available, register the waker and return Pending.
            let state_clone = Rc::clone(&this.state);
            {
                this.state.borrow_mut().waker = Some(cx.waker().clone());
            }
            // Add the state to the acquire queue.
            {
                this.allocator.acquire_queue.borrow_mut().push_back(state_clone);
            }

            Poll::Pending
        }
    }
}

impl LazyAcquire {
    pub fn new(allocator: Rc<FixedBufferAllocator>) -> Self {
        LazyAcquire {
            state: Rc::new(RefCell::new(LazyAcquireState { waker: None })),
            allocator,
        }
    }
}

struct LazyAcquireState {
    waker: Option<std::task::Waker>,
}

/// Allocator for fixed buffers registered with io_uring.
pub struct FixedBufferAllocator {
    // Each buffer is wrapped in a Mutex to allow mutable access concurrently.
    buffers: RefCell<Vec<FixedBufferInner>>,

    acquire_queue: RefCell<VecDeque<Rc<RefCell<LazyAcquireState>>>>,
}

impl FixedBufferAllocator {
    /// Creates a new allocator with `queue_size` buffers of `buffer_size` each.
    /// The buffers are registered with the provided `ring` as iovecs.
    pub fn new(queue_size: usize, buffer_size: usize, ring: &mut IoUring) -> Rc<Self> {
        let mut buffers = Vec::with_capacity(queue_size);
        for i in 0..queue_size {
            let buf = aligned_alloc(buffer_size);
            let fixed_buf = FixedBufferInner {
                buffer: ManuallyDrop::new(buf),
                index: i,
            };
            buffers.push(fixed_buf);
        }
        // Build iovecs from the preallocated buffers.
        let iovecs: Vec<iovec> = buffers
            .iter()
            .map(|fixed_buf| {
                let buf = fixed_buf.buffer.as_ref();
                iovec {
                    iov_base: buf.as_ptr() as *mut _,
                    iov_len: buf.len(),
                }
            })
            .collect();
        // Register the buffers with io_uring.
        unsafe {
            ring.submitter()
                .register_buffers(&iovecs)
                .expect("Failed to register buffers");
        }

        Rc::new(FixedBufferAllocator {
            buffers: RefCell::new(buffers),
            acquire_queue: RefCell::new(VecDeque::new()),
        })
    }

    /// Acquires an available buffer. Returns a WriteFixedBuffer handle.
    pub async fn acquire(self: &Rc<Self>) -> FixedBuffer {
        LazyAcquire::new(Rc::clone(self)).await
    }

    fn acquire_inner(self: &Rc<Self>) -> Option<FixedBuffer> {
        let mut buffers = self.buffers.borrow_mut();
        buffers.pop().map(|fixed_buf| {
            let buffer = FixedBuffer {
                buffer: Some(fixed_buf),
                allocator: Rc::clone(self),
            };
            buffer
        })
    }

    // pub fn grow(&mut self, ring: &mut IoUring) {
    //     // unregister buffers
    //     ring.submitter()
    //         .unregister_buffers()
    //         .expect("Failed to unregister buffers");

    //     // grow buffers
    //     let current_len = self.buffers.len();
    //     for _ in 0..current_len {
    //         let buf = aligned_alloc(4096);
    //         self.buffers.push(RefCell::new(ManuallyDrop::new(buf)));
    //     }
    //     // Build iovecs from the preallocated buffers.
    //     let iovecs: Vec<iovec> = self
    //         .buffers
    //         .iter()
    //         .map(|buf_mutex| {
    //             let buf = buf_mutex.borrow_mut();
    //             iovec {
    //                 iov_base: buf.as_ptr() as *mut _,
    //                 iov_len: buf.len(),
    //             }
    //         })
    //         .collect();
    //     // Register the buffers with io_uring.
    //     unsafe {
    //         ring.submitter()
    //             .register_buffers(&iovecs)
    //             .expect("Failed to register buffers");
    //     }
    // }

    // Grants mutable access to the buffer by its index.
    // pub fn get_buffer_mut(&self, index: usize) -> std::cell::RefMut<Box<[u8]>> {
    //     std::cell::RefMut::map(self.buffers[index].borrow_mut(), |buf| &mut **buf)
    // }

    pub fn used_buffers(&self) -> f64 {
        let (total, free) = {
            let binding = self.buffers.borrow();
            (binding.len(), binding.capacity())
        };
        (total - free) as f64 / total as f64 * 100.0
    }

    pub fn fill_buffers(&self, data: u8) {
        let mut binding = self.buffers.borrow_mut();
        for buf in binding.iter_mut() {
            let buf = buf.buffer.as_mut();
            for i in 0..buf.len() {
                buf[i] = data;
            }
        }
    }
}

impl Drop for FixedBufferAllocator {
    fn drop(&mut self) {
        // Drop the buffers.
        while let Some(mut buffer) = self.buffers.borrow_mut().pop() {
            // Manually drop the buffer to avoid double free.
            unsafe {
                ManuallyDrop::drop(&mut buffer.buffer);
            }
        }
    }
}

// /// Handle for a fixed buffer used in WriteFixed operations. When dropped,
// /// it returns the buffer back to the allocator.
// pub struct WriteFixedBuffer {
//     index: usize,
//     allocator: Rc<FixedBufferAllocator>,
// }

// impl WriteFixedBuffer {
//     // Returns a mutable slice to the underlying buffer.
//     pub fn as_mut_slice(&self) -> std::cell::RefMut<'_, Box<[u8]>> {
//         self.allocator.get_buffer_mut(self.index)
//     }

//     /// Prepares a WriteFixed SQE for a file descriptor using this buffer.
//     /// (Additional fields like offset and length would be set here.)
//     pub fn prepare_sqe(&self, fd: RawFd, offset: u64) -> io_uring::squeue::Entry {
//         // Example using io_uring opcode WriteFixed.
//         // The index registered via io_uring_register is passed in user_data.
//         opcode::WriteFixed::new(
//             types::Fd(fd),
//             self.allocator.buffers[self.index].borrow().as_ptr() as *const _,
//             // Assuming the full length should be written.
//             self.allocator.buffers[self.index].borrow().len() as u32,
//             self.index as u16,
//         )
//         .offset(offset)
//         .build()
//     }
// }

// impl Drop for WriteFixedBuffer {
//     fn drop(&mut self) {
//         // On drop, the buffer is marked as free.
//         self.allocator.release(self.index);
//     }
// }

pub struct FixedBuffer {
    pub buffer: Option<FixedBufferInner>,
    pub allocator: Rc<FixedBufferAllocator>,
}

pub struct FixedBufferInner {
    pub buffer: ManuallyDrop<Box<[u8]>>,
    pub index: usize,
}

impl FixedBuffer {
    // pub fn as_mut_slice(&self) -> std::cell::RefMut<'_, Box<[u8]>> {
    //     self.allocator.get_buffer_mut(self.index)
    // }

    pub fn as_ptr(&self) -> *const u8 {
        self.buffer.as_ref().map_or(std::ptr::null(), |buf| {
            buf.buffer.as_ptr()
        })
    }

    pub fn len(&self) -> usize {
        self.buffer.as_ref().map_or(0, |buf| buf.buffer.len())
    }

    pub fn index(&self) -> usize {
        self.buffer.as_ref().map_or(0, |buf| buf.index)
    }
}

impl Drop for FixedBuffer {
    fn drop(&mut self) {
        // On drop, the buffer is marked as free.
        // self.allocator.release(self.index);
        {
            let mut binding = self.allocator.buffers.borrow_mut();
            if let Some(buffer) = self.buffer.take() {
                binding.push(buffer);
            }
        }
        // Notify any waiting tasks that a buffer is now available.
        let mut acquire_queue = self.allocator.acquire_queue.borrow_mut();
        if let Some(state) = acquire_queue.pop_front() {
            if let Some(waker) = state.borrow_mut().waker.take() {
                waker.wake();
            }
        }
    }
}


fn aligned_alloc(size: usize) -> Box<[u8]> {
    unsafe {
        // 余りを切り上げながら4096で割る
        let size = (size + ALIGN - 1) / ALIGN;
        // メモリ確保
        let mut vec = Vec::<AlignedBuffer>::with_capacity(size);
        // データの書き込み
        vec.resize_with(size, Default::default);
        // アライン付き確保が終わったのでデータをu8にする
        let mut data = std::mem::transmute::<_, Vec<u8>>(vec);
        // そのままだと4096分の1の要素しかないのでメタデータを変更する
        data.set_len(size * ALIGN);
        data.into_boxed_slice()
    }
}
