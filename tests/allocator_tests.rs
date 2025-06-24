use pluvio::reactor::allocator::FixedBufferAllocator;
use io_uring::IoUring;
use futures::executor::block_on;

#[test]
fn buffer_usage_changes() {
    let mut ring = IoUring::builder().build(2).expect("create io_uring");
    let allocator = FixedBufferAllocator::new(2, 64, &mut ring);

    // no buffers acquired yet
    assert_eq!(allocator.used_buffers(), 0.0);

    // acquire one buffer
    let buf = block_on(allocator.acquire());
    assert_eq!(allocator.used_buffers(), 50.0);

    // release buffer
    drop(buf);
    assert_eq!(allocator.used_buffers(), 0.0);

    // acquire all buffers
    let _b1 = block_on(allocator.acquire());
    let _b2 = block_on(allocator.acquire());
    assert_eq!(allocator.used_buffers(), 100.0);
    // buffers released on drop when scope ends
}
