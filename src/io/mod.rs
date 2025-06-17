use crate::reactor::allocator::{FixedBuffer, FixedBufferAllocator};
use std::rc::Rc;

pub mod file;

pub async fn prepare_buffer(allocator: Rc<FixedBufferAllocator>) -> FixedBuffer {
    allocator.acquire().await
}
