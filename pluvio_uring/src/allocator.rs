//! Fixed buffer allocator used with `io_uring` operations.
//!
//! Buffers are pre-registered with the kernel so operations such as
//! [`ReadFixed`](io_uring::opcode::ReadFixed) can be used efficiently.

use aligned_box::AlignedBox;
use std::{
    alloc::{Layout, alloc_zeroed, handle_alloc_error},
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

/// Future returned by [`FixedBufferAllocator::acquire`] to lazily obtain a buffer.
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
            // Log buffer exhaustion for performance diagnostics
            let (total, free, waiting) = {
                let buffers = this.allocator.buffers.borrow();
                let queue = this.allocator.acquire_queue.borrow();
                (buffers.capacity(), buffers.len(), queue.len())
            };
            tracing::debug!(
                total_buffers = total,
                free_buffers = free,
                waiting_tasks = waiting,
                "Buffer pool exhausted, task waiting for buffer"
            );

            let state_clone = Rc::clone(&this.state);
            {
                this.state.borrow_mut().waker = Some(cx.waker().clone());
            }
            // Add the state to the acquire queue.
            {
                this.allocator
                    .acquire_queue
                    .borrow_mut()
                    .push_back(state_clone);
            }

            Poll::Pending
        }
    }
}

impl LazyAcquire {
    /// Create a new [`LazyAcquire`] associated with an allocator.
    pub fn new(allocator: Rc<FixedBufferAllocator>) -> Self {
        LazyAcquire {
            state: Rc::new(RefCell::new(LazyAcquireState { waker: None })),
            allocator,
        }
    }
}

/// Internal state held for tasks waiting on a buffer.
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
    #[tracing::instrument(level = "trace", skip(ring))]
    pub fn new(queue_size: usize, buffer_size: usize, ring: &mut IoUring) -> Rc<Self> {
        let allocator = Self::new_without_uring(queue_size, buffer_size);
        // Build iovecs from the preallocated buffers and register with io_uring.
        let buffers = allocator.buffers.borrow();
        let iovecs: Vec<iovec> = buffers
            .iter()
            .map(|fixed_buf| {
                let slice = fixed_buf.as_slice();
                iovec {
                    iov_base: slice.as_ptr() as *mut _,
                    iov_len: slice.len(),
                }
            })
            .collect();
        unsafe {
            ring.submitter()
                .register_buffers(&iovecs)
                .expect("Failed to register buffers");
        }
        drop(buffers);
        allocator
    }

    /// Allocates `queue_size` page-aligned buffers of `buffer_size` bytes each
    /// **without** registering them with io_uring.
    ///
    /// Useful when the buffers need to be shared with another subsystem that
    /// also wants to pin them (e.g. RDMA via `register_rdma_keys`) before
    /// io_uring takes them — or when io_uring registration is not needed
    /// at all (pure RDMA pool).
    ///
    /// Memory layout is identical to [`Self::new`], so a follow-up
    /// `io_uring::Submitter::register_buffers` call against the iovecs
    /// returned by [`Self::iovecs`] (TODO if needed) is equivalent.
    pub fn new_without_uring(queue_size: usize, buffer_size: usize) -> Rc<Self> {
        let mut buffers = Vec::with_capacity(queue_size);
        let page_size = page_size();
        for i in 0..queue_size {
            let buf = new_aligned_buffer(page_size, buffer_size);
            buffers.push(FixedBufferInner {
                buffer: ManuallyDrop::new(buf),
                index: i,
                rdma_lkey: 0,
                rdma_rkey: 0,
            });
        }
        Rc::new(FixedBufferAllocator {
            buffers: RefCell::new(buffers),
            acquire_queue: RefCell::new(VecDeque::new()),
        })
    }

    /// Apply RDMA registration to every buffer in the pool.
    ///
    /// `register_fn(ptr, len)` is called once per buffer and must return
    /// `(lkey, rkey)` for that buffer's MR. The caller is responsible for
    /// keeping the resulting `MemoryRegion` (or equivalent) alive for the
    /// lifetime of this allocator — pluvio_uring intentionally does not
    /// depend on any RDMA library, so the MR lifetime sits with the caller.
    ///
    /// After this call, [`FixedBuffer::rdma_lkey`] / [`FixedBuffer::rdma_rkey`]
    /// return the registered keys for the buffer.
    ///
    /// All buffers must be free (none currently lent out). This is
    /// expected to be called once, immediately after construction.
    pub fn register_rdma_keys<E, F>(&self, mut register_fn: F) -> Result<(), E>
    where
        F: FnMut(*mut u8, usize) -> Result<(u32, u32), E>,
    {
        let mut buffers = self.buffers.borrow_mut();
        for buf in buffers.iter_mut() {
            let slice = buf.as_slice();
            let ptr = slice.as_ptr() as *mut u8;
            let len = slice.len();
            let (lkey, rkey) = register_fn(ptr, len)?;
            buf.rdma_lkey = lkey;
            buf.rdma_rkey = rkey;
        }
        Ok(())
    }

    /// Acquires an available buffer. Returns a WriteFixedBuffer handle.
    /// Acquire a buffer asynchronously.
    #[async_backtrace::framed]
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn acquire(self: &Rc<Self>) -> FixedBuffer {
        LazyAcquire::new(Rc::clone(self)).await
    }

    /// Try to acquire a buffer without waiting. Returns `None` if the pool
    /// is empty.
    pub fn try_acquire(self: &Rc<Self>) -> Option<FixedBuffer> {
        self.acquire_inner()
    }

    /// Try to acquire a buffer without waiting.
    fn acquire_inner(self: &Rc<Self>) -> Option<FixedBuffer> {
        let mut buffers = self.buffers.borrow_mut();
        let total = buffers.capacity();
        buffers.pop().map(|fixed_buf| {
            let free_after = buffers.len();
            let used = total - free_after;
            let utilization = if total > 0 {
                (used as f64 / total as f64) * 100.0
            } else {
                0.0
            };
            // Log buffer utilization at trace level for detailed analysis
            tracing::trace!(
                total_buffers = total,
                free_buffers = free_after,
                used_buffers = used,
                utilization_pct = %format!("{:.1}", utilization),
                "Buffer acquired"
            );
            FixedBuffer {
                buffer: Some(fixed_buf),
                allocator: Rc::clone(self),
            }
        })
    }

    /// Percentage of buffers currently in use.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn used_buffers(&self) -> f64 {
        let (total, free) = {
            let binding = self.buffers.borrow();
            (binding.capacity(), binding.len())
        };
        if total == 0 {
            0.0
        } else {
            (total - free) as f64 / total as f64 * 100.0
        }
    }

    /// Fill all buffers with the provided byte value.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn fill_buffers(&self, data: u8) {
        let mut binding = self.buffers.borrow_mut();
        for buf in binding.iter_mut() {
            for byte in buf.as_mut_slice().iter_mut() {
                *byte = data;
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

#[cfg(unix)]
fn page_size() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(n > 0, "sysconf(_SC_PAGESIZE) failed");
    n as usize
}

/// Handle for a registered fixed buffer.
pub struct FixedBuffer {
    pub buffer: Option<FixedBufferInner>,
    pub allocator: Rc<FixedBufferAllocator>,
}

/// Inner representation of a fixed buffer returned to the allocator.
pub struct FixedBufferInner {
    pub buffer: ManuallyDrop<AlignedBox<[u8]>>,
    pub index: usize,
    /// Optional RDMA registration keys.
    ///
    /// Set by [`FixedBufferAllocator::register_rdma_keys`]; zero when the
    /// buffer has not been registered with an HCA. The lifetime of the
    /// underlying MR is owned by the caller of `register_rdma_keys`.
    pub rdma_lkey: u32,
    pub rdma_rkey: u32,
}

impl FixedBufferInner {
    pub fn as_slice(&self) -> &[u8] {
        &**self.buffer
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut **self.buffer
    }
}

impl FixedBuffer {
    // pub fn as_mut_slice(&self) -> std::cell::RefMut<'_, Box<[u8]>> {
    //     self.allocator.get_buffer_mut(self.index)
    // }

    /// Pointer to the start of the buffer.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn as_ptr(&self) -> *const u8 {
        self.buffer
            .as_ref()
            .map_or(std::ptr::null(), |buf| buf.as_slice().as_ptr())
    }

    /// Length of the buffer in bytes.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn len(&self) -> usize {
        self.buffer.as_ref().map_or(0, |buf| buf.as_slice().len())
    }

    /// Index of this buffer within the allocator.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn index(&self) -> usize {
        self.buffer.as_ref().map_or(0, |buf| buf.index)
    }

    /// Mutable slice to the underlying memory region.
    #[tracing::instrument(level = "trace", skip(self))]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.buffer
            .as_mut()
            .map_or(&mut [], |buf| buf.as_mut_slice())
    }

    /// Raw mutable pointer to the buffer's start, callable with `&self`.
    ///
    /// Safe to obtain (the AlignedBox memory is pinned by `ManuallyDrop`),
    /// but caller must not race writes via this pointer with `as_mut_slice`
    /// readers. Intended for handing the buffer address to subsystems that
    /// will write into it asynchronously (RDMA HCA, io_uring kernel).
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.as_ptr() as *mut u8
    }

    /// RDMA local key for this buffer (0 if never RDMA-registered).
    pub fn rdma_lkey(&self) -> u32 {
        self.buffer.as_ref().map_or(0, |b| b.rdma_lkey)
    }

    /// RDMA remote key for this buffer (0 if never RDMA-registered).
    pub fn rdma_rkey(&self) -> u32 {
        self.buffer.as_ref().map_or(0, |b| b.rdma_rkey)
    }
}

impl AsRef<[u8]> for FixedBuffer {
    fn as_ref(&self) -> &[u8] {
        self.buffer
            .as_ref()
            .map_or(&[], |buf| buf.as_slice())
    }
}

impl AsMut<[u8]> for FixedBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        self.buffer
            .as_mut()
            .map_or(&mut [], |buf| buf.as_mut_slice())
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
        // Extract the waker while holding the lock, then release the lock before waking.
        // This prevents deadlock when the woken task immediately tries to acquire a buffer
        // and needs to access acquire_queue in LazyAcquire::poll().
        let waker_to_wake = {
            let mut acquire_queue = self.allocator.acquire_queue.borrow_mut();
            acquire_queue.pop_front().and_then(|state| {
                state.borrow_mut().waker.take()
            })
        };
        // Notify any waiting tasks that a buffer is now available.
        // The lock is released before calling wake() to avoid deadlock.
        if let Some(waker) = waker_to_wake {
            waker.wake();
        }
    }
}

fn new_aligned_buffer(alignment: usize, len: usize) -> AlignedBox<[u8]> {
    assert!(
        len > 0,
        "buffer_size must be greater than zero when allocating fixed buffers"
    );

    let layout = Layout::from_size_align(len, alignment)
        .unwrap_or_else(|err| panic!("invalid layout for aligned buffer: {err:?}"));

    unsafe {
        let ptr = alloc_zeroed(layout);
        if ptr.is_null() {
            handle_alloc_error(layout);
        }

        let slice_ptr = std::ptr::slice_from_raw_parts_mut(ptr, len);
        AlignedBox::<[u8]>::from_raw_parts(slice_ptr, layout)
    }
}
