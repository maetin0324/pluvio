use std::{
  cell::RefCell, mem::ManuallyDrop, os::unix::io::RawFd, rc::Rc,
};

use io_uring::{opcode, types, IoUring};
use libc::iovec;

pub const ALIGN: usize = 4096;
#[repr(align(4096))]
pub struct AlignedBuffer{
    pub data: [u8; 4096]
}

impl Default for AlignedBuffer {
    fn default() -> Self {
        AlignedBuffer{data: [0; 4096]}
    }
}

/// Allocator for fixed buffers registered with io_uring.
pub struct FixedBufferAllocator {
  free_indices: RefCell<Vec<usize>>,
  // Each buffer is wrapped in a Mutex to allow mutable access concurrently.
  buffers: Vec<RefCell<ManuallyDrop<Box<[u8]>>>>,
}

impl FixedBufferAllocator {
  /// Creates a new allocator with `queue_size` buffers of `buffer_size` each.
  /// The buffers are registered with the provided `ring` as iovecs.
  pub fn new(queue_size: usize, buffer_size: usize, ring: &mut IoUring) -> Rc<Self> {
      let mut buffers = Vec::with_capacity(queue_size);
      for _ in 0..queue_size {
          let buf = aligned_alloc(buffer_size);
          buffers.push(RefCell::new(ManuallyDrop::new(buf)));
      }
      // Build iovecs from the preallocated buffers.
      let iovecs: Vec<iovec> = buffers
          .iter()
          .map(|buf_mutex| {
              let buf = buf_mutex.borrow_mut();
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
      // All indices are initially free.
      let free_indices = (0..queue_size).rev().collect();
      Rc::new(FixedBufferAllocator {
          free_indices: RefCell::new(free_indices),
          buffers,
      })
  }

  /// Acquires an available buffer. Returns a WriteFixedBuffer handle.
  pub fn acquire(self: &Rc<Self>) -> Option<WriteFixedBuffer> {
      let mut free = self.free_indices.borrow_mut();
      free.pop().map(|index| WriteFixedBuffer {
          index,
          allocator: Rc::clone(self),
      })
  }

  /// Returns a buffer (given by its index) back to the free pool.
  fn release(&self, index: usize) {
      let mut free = self.free_indices.borrow_mut();
      free.push(index);
  }

  // Grants mutable access to the buffer by its index.
pub fn get_buffer_mut(&self, index: usize) -> std::cell::RefMut<Box<[u8]>> {
      std::cell::RefMut::map(self.buffers[index].borrow_mut(), |buf| &mut **buf)
}
}

/// Handle for a fixed buffer used in WriteFixed operations. When dropped,
/// it returns the buffer back to the allocator.
pub struct WriteFixedBuffer {
  index: usize,
  allocator: Rc<FixedBufferAllocator>,
}

impl WriteFixedBuffer {
  // Returns a mutable slice to the underlying buffer.
  pub fn as_mut_slice(&self) -> std::cell::RefMut<'_, Box<[u8]>> {
      self.allocator.get_buffer_mut(self.index)
  }

  /// Prepares a WriteFixed SQE for a file descriptor using this buffer.
  /// (Additional fields like offset and length would be set here.)
  pub fn prepare_sqe(&self, fd: RawFd, offset: u64) -> io_uring::squeue::Entry {
      // Example using io_uring opcode WriteFixed.
      // The index registered via io_uring_register is passed in user_data.
      opcode::WriteFixed::new(
          types::Fd(fd),
          self.allocator.buffers[self.index]
              .borrow()
              .as_ptr() as *const _,
          // Assuming the full length should be written.
          self.allocator.buffers[self.index].borrow().len() as u32,
          self.index as u16,
      )
      .offset(offset)
      .build()
  }
}

impl Drop for WriteFixedBuffer {
  fn drop(&mut self) {
      // On drop, the buffer is marked as free.
      self.allocator.release(self.index);
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