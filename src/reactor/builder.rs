//! Builder for [`IoUringReactor`](crate::reactor::IoUringReactor).
//!
//! Allows configuring queue sizes and timeouts before constructing the reactor.

use std::{cell::{Cell, RefCell}, collections::HashMap, rc::Rc, sync::atomic::AtomicU64, time::Duration};

use io_uring::IoUring;

use crate::reactor::{allocator::FixedBufferAllocator, IoUringParams, IoUringReactor};

/// Configures parameters for a new [`IoUringReactor`].
pub struct IoUringReactorBuilder {
    queue_size: u32,
    buffer_size: usize,
    submit_depth: u32,
    wait_submit_timeout: Duration,
    wait_complete_timeout: Duration,
}

impl Default for IoUringReactorBuilder {
    fn default() -> Self {
        IoUringReactorBuilder {
            queue_size: 1024,
            buffer_size: 4096,
            submit_depth: 64,
            wait_submit_timeout: Duration::from_millis(100),
            wait_complete_timeout: Duration::from_millis(1000),
        }
    }
}

impl IoUringReactorBuilder {
    /// Create a new builder using default parameters.
    pub fn new() -> Self {
        IoUringReactorBuilder::default()
    }

    /// Set the size of the io_uring queue.
    pub fn queue_size(mut self, size: u32) -> Self {
        self.queue_size = size;
        self
    }

    /// Set the size of each pre-allocated buffer.
    pub fn buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// Configure how many SQEs are submitted at once.
    pub fn submit_depth(mut self, depth: u32) -> Self {
        self.submit_depth = depth;
        self
    }

    /// Timeout before automatically submitting queued SQEs.
    pub fn wait_submit_timeout(mut self, timeout: Duration) -> Self {
        self.wait_submit_timeout = timeout;
        self
    }

    /// Timeout before waiting for completions.
    pub fn wait_complete_timeout(mut self, timeout: Duration) -> Self {
        self.wait_complete_timeout = timeout;
        self
    }

    /// Build the [`IoUringReactor`] from the configured parameters.
    pub fn build(self) -> Rc<IoUringReactor> {
        let ring = IoUring::builder()
            // .setup_iopoll()
            .build(self.queue_size)
            .expect("Failed to create io_uring");
        if ring.params().is_feature_nodrop() {
            tracing::trace!("io_uring supports IORING_FEAT_NODROP");
        } else {
            tracing::trace!("io_uring does not support IORING_FEAT_NODROP");
        }

        let ring = Rc::new(RefCell::new(ring));

        let allocator =
            FixedBufferAllocator::new((self.queue_size) as usize, self.buffer_size, &mut ring.borrow_mut());

        Rc::new(IoUringReactor {
            ring: ring,
            completions: Rc::new(RefCell::new(HashMap::new())),
            user_data_counter: AtomicU64::new(0),
            allocator: allocator,
            last_submit_time: RefCell::new(std::time::Instant::now()),
            io_uring_params: IoUringParams {
                submit_depth: self.submit_depth,
                wait_submit_timeout: self.wait_submit_timeout,
                wait_complete_timeout: self.wait_complete_timeout,
            },
            completed_count: Cell::new(0),
        })
    }
}