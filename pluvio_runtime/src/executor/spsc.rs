//! Single-thread SPSC channel implemented with an UnsafeCell-backed ring buffer.
//! Drop-in surface for a subset of `crossbeam_channel` used by Pluvio.

use std::cell::{Cell, UnsafeCell};
use std::mem::MaybeUninit;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

// ===== Public error types modeled after crossbeam-channel =====

#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
}

#[derive(Debug, PartialEq, Eq)]
pub struct RecvError;

#[derive(Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Disconnected,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendTimeoutError<T> {
    Timeout(T),
    Disconnected(T),
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecvTimeoutError {
    Timeout,
    Disconnected,
}

// ===== Channel halves (single-thread, !Send/!Sync by construction) =====

#[derive(Debug)]
pub struct Sender<T> {
    inner: Rc<Inner<T>>,
}
#[derive(Debug)]
pub struct Receiver<T> {
    inner: Rc<Inner<T>>,
}

#[derive(Debug)]
struct Inner<T> {
    // Ring buffer storage (length = buf_len). We keep one empty slot as a sentinel.
    buf: UnsafeCell<Box<[MaybeUninit<T>]>>,
    // Actual buffer length (including the extra sentinel slot). Mutable due to growth for `unbounded`.
    buf_len: Cell<usize>,
    // Index of next write / next read (mod buf_len)
    head: Cell<usize>,
    tail: Cell<usize>,
    // Channel mode
    unbounded: bool,
    // Liveness flags
    sender_alive: Cell<bool>,
    receiver_alive: Cell<bool>,
}

impl<T> Inner<T> {
    fn new_bounded(cap: usize) -> Self {
        assert!(cap > 0, "cap must be > 0");
        let buf_len = cap + 1; // sentinel slot to distinguish full vs empty
                               // MaybeUninitがCloneを実装していないため、vec!マクロが使えない
        let mut buf = Vec::with_capacity(buf_len);
        buf.resize_with(buf_len, MaybeUninit::uninit);
        let buf = buf.into_boxed_slice();
        Self {
            buf: UnsafeCell::new(buf),
            buf_len: Cell::new(buf_len),
            head: Cell::new(0),
            tail: Cell::new(0),
            unbounded: false,
            sender_alive: Cell::new(true),
            receiver_alive: Cell::new(true),
        }
    }

    fn new_unbounded(initial_cap: usize) -> Self {
        let cap = initial_cap.max(1);
        let buf_len = cap + 1;
        let mut buf = Vec::with_capacity(buf_len);
        buf.resize_with(buf_len, MaybeUninit::uninit);
        let buf = buf.into_boxed_slice();
        Self {
            buf: UnsafeCell::new(buf),
            buf_len: Cell::new(buf_len),
            head: Cell::new(0),
            tail: Cell::new(0),
            unbounded: true,
            sender_alive: Cell::new(true),
            receiver_alive: Cell::new(true),
        }
    }

    /// Authoritative buffer length: read directly from the boxed slice's
    /// fat-pointer length. We previously cached this in a separate `Cell`,
    /// but that introduced a TOCTOU window during `try_grow` (the buffer
    /// is replaced before the cached size is updated) and made it
    /// impossible to detect external heap corruption of the `Box`
    /// fat-pointer. Reading the slice length each time is one extra
    /// load and is single-thread safe (only the thread that holds `&self`
    /// can mutate `buf`).
    #[inline]
    fn blen(&self) -> usize {
        // SAFETY: single-thread; `&self` access; no &mut to `*buf.get()`
        // is held across this read. Borrow the boxed slice explicitly
        // (avoid `(*self.buf.get()).len()` which Clippy flags as an
        // implicit autoref through a raw pointer).
        unsafe { (&*self.buf.get()).len() }
    }
    #[inline]
    fn inc(&self, i: usize) -> usize {
        let m = self.blen();
        // Defensive: if `head`/`tail` ever exceed `blen()` (e.g. the
        // boxed-slice length was clobbered by external heap corruption),
        // a naïve `i + 1 == m` test wraps incorrectly and we keep
        // incrementing forever. Always reduce mod `m` first so the ring
        // semantics are preserved.
        if m == 0 {
            return 0;
        }
        let i_mod = i % m;
        if i_mod + 1 == m {
            0
        } else {
            i_mod + 1
        }
    }
    #[inline]
    fn is_empty(&self) -> bool {
        self.head.get() == self.tail.get()
    }
    #[inline]
    fn is_full(&self) -> bool {
        self.inc(self.head.get()) == self.tail.get()
    }
    #[inline]
    fn cap(&self) -> usize {
        self.blen() - 1
    }
    #[inline]
    fn len(&self) -> usize {
        let h = self.head.get();
        let t = self.tail.get();
        let m = self.blen();
        if m == 0 {
            return 0;
        }
        let h = h % m;
        let t = t % m;
        if h >= t {
            h - t
        } else {
            m - t + h
        }
    }

    fn try_grow(&self) {
        if !self.unbounded || !self.is_full() {
            return;
        }
        let old_len = self.blen();
        let old_cap = old_len - 1;
        let new_cap = old_cap.saturating_mul(2).max(2);
        let new_len = new_cap + 1;
        let mut new_buf = Vec::with_capacity(new_len);
        new_buf.resize_with(new_len, MaybeUninit::uninit);
        let mut new_buf = new_buf.into_boxed_slice();
        // Move elements in order [tail .. tail+count) into [0 .. count)
        // of the new buffer. `inc` here still reads the OLD length via
        // `blen() = (*self.buf.get()).len()` because `self.buf` has not
        // been swapped yet.
        let mut t = self.tail.get();
        let count = self.len();
        for i in 0..count {
            let v = unsafe { (&mut *self.buf.get())[t].assume_init_read() };
            new_buf[i].write(v);
            t = self.inc(t);
        }
        // Reset head/tail BEFORE swapping the buffer so that any
        // subsequent `blen()` (which now returns the new buffer length)
        // sees consistent indices. Writing `head`/`tail` to values that
        // fit the OLD length is safe — after the swap they fit the new
        // length too (count < new_len since new_len = 2*old_cap + 1).
        self.tail.set(0);
        self.head.set(count);
        // Replace the buffer last. The OLD Box's drop deallocates the
        // old heap region; its `MaybeUninit<T>` cells are no-op-drop so
        // no double-free of the moved-out `T`s.
        unsafe {
            *self.buf.get() = new_buf;
        }
        // `buf_len` field is kept around for diagnostics only; it MUST
        // mirror `(*self.buf.get()).len()` after this point. `blen()`
        // reads the authoritative value directly from the slice so a
        // mismatch can only matter for someone inspecting `buf_len`.
        self.buf_len.set(new_len);
    }

    /// # Safety
    ///
    /// Caller must ensure the slot at `idx % blen` is uninitialized
    /// (i.e. either never written or already read via `read_at`). The
    /// index is reduced modulo the actual buffer length so that a
    /// stale `head` value never reaches the boxed-slice bounds check —
    /// this used to abort the process at the first `head ≥ buf.len()`
    /// with a generic "index out of bounds" panic that hid the real
    /// invariant violation.
    #[inline]
    unsafe fn write_at(&self, idx: usize, val: T) {
        let buf = &mut *self.buf.get();
        let m = buf.len();
        debug_assert!(m > 0, "SPSC buffer is empty (len=0)");
        let i = idx % m;
        buf[i].write(val);
    }
    /// # Safety
    ///
    /// Caller must ensure the slot at `idx % blen` is initialized
    /// (between `tail` and `head` in the ring's logical order). See
    /// [`write_at`] for the modulus rationale.
    #[inline]
    unsafe fn read_at(&self, idx: usize) -> T {
        let buf = &mut *self.buf.get();
        let m = buf.len();
        debug_assert!(m > 0, "SPSC buffer is empty (len=0)");
        let i = idx % m;
        buf[i].assume_init_read()
    }
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        // Drop any remaining initialized elements. We reduce the index
        // modulo the actual buffer length on every iteration so a stale
        // tail value can't index past the boxed slice. `head` is also
        // reduced for the loop condition.
        let m = self.buf.get_mut().len();
        if m == 0 {
            return;
        }
        let head = self.head.get() % m;
        let mut tail = self.tail.get() % m;
        while tail != head {
            // SAFETY: we have &mut self; elements in [tail, head) are
            // initialized, indices are reduced to [0, m).
            unsafe {
                self.buf.get_mut()[tail].assume_init_drop();
            }
            tail = (tail + 1) % m;
        }
        self.tail.set(head);
    }
}

/// Create a bounded channel.
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(Inner::new_bounded(cap));
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

/// Create an unbounded channel (auto-growing ring).
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    const DEFAULT_INIT: usize = 32;
    let inner = Rc::new(Inner::new_unbounded(DEFAULT_INIT));
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

impl<T> Sender<T> {
    /// Non-blocking send.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        if !self.inner.receiver_alive.get() {
            return Err(TrySendError::Disconnected(msg));
        }
        if self.inner.is_full() {
            self.inner.try_grow();
        }
        if self.inner.is_full() {
            return Err(TrySendError::Full(msg));
        }

        let h = self.inner.head.get();
        unsafe {
            self.inner.write_at(h, msg);
        }
        self.inner.head.set(self.inner.inc(h));
        Ok(())
    }

    /// "Blocking" send with brief spin/yield. Prefer `try_send` in single-thread loops.
    pub fn send(&self, mut msg: T) -> Result<(), SendError<T>> {
        match self.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(m)) => Err(SendError(m)),
            Err(TrySendError::Full(m)) => {
                msg = m;
                Err(SendError(msg))
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub fn is_disconnected(&self) -> bool {
        !self.inner.receiver_alive.get()
    }
    pub fn capacity(&self) -> usize {
        self.inner.cap()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.sender_alive.set(false);
    }
}

impl<T> Receiver<T> {
    /// Non-blocking receive.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        if self.inner.is_empty() {
            if !self.inner.sender_alive.get() {
                return Err(TryRecvError::Disconnected);
            }
            return Err(TryRecvError::Empty);
        }
        let t = self.inner.tail.get();
        let v = unsafe { self.inner.read_at(t) };
        self.inner.tail.set(self.inner.inc(t));
        Ok(v)
    }

    /// Spinning receive for API compatibility.
    pub fn recv(&self) -> Result<T, RecvError> {
        loop {
            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Disconnected) => return Err(RecvError),
                Err(TryRecvError::Empty) => thread::yield_now(),
            }
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        self.recv_deadline(deadline)
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        loop {
            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return Err(RecvTimeoutError::Timeout);
                    }
                    thread::yield_now();
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub fn is_disconnected(&self) -> bool {
        !self.inner.sender_alive.get()
    }
    pub fn capacity(&self) -> usize {
        self.inner.cap()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.receiver_alive.set(false);
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender {
            inner: self.inner.clone(),
        }
    }
}
impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Receiver {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_channel_preserves_fifo_and_capacity() {
        let (sender, receiver) = bounded(2);
        assert_eq!(sender.capacity(), 2);
        assert!(sender.is_empty());

        sender.try_send(10).unwrap();
        sender.try_send(20).unwrap();

        let err = sender.try_send(30).unwrap_err();
        assert!(matches!(err, TrySendError::Full(30)));
        assert_eq!(sender.len(), 2);

        assert_eq!(receiver.try_recv().unwrap(), 10);
        sender.try_send(30).unwrap();
        assert_eq!(receiver.try_recv().unwrap(), 20);
        assert_eq!(receiver.try_recv().unwrap(), 30);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn unbounded_channel_grows_buffer() {
        let (sender, receiver) = unbounded();
        let initial_capacity = sender.capacity();
        assert!(initial_capacity >= 1);

        let total_messages = initial_capacity + 5;
        for i in 0..total_messages {
            sender.try_send(i).unwrap();
        }

        assert!(
            sender.capacity() > initial_capacity,
            "capacity did not grow: {}",
            sender.capacity()
        );
        assert_eq!(sender.len(), total_messages);

        for i in 0..total_messages {
            assert_eq!(receiver.try_recv().unwrap(), i);
        }
        assert!(receiver.is_empty());
    }

    #[test]
    fn disconnect_propagates_errors() {
        let (sender, receiver) = bounded::<i32>(1);
        drop(receiver);
        let err = sender.try_send(42).unwrap_err();
        assert!(matches!(err, TrySendError::Disconnected(42)));

        let (sender, receiver) = bounded::<i32>(1);
        drop(sender);
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }
}
