//! Active-message router: demultiplexes a single AM stream into per-`(rank, step, phase)`
//! receive slots.
//!
//! Each `recv_from(rank, step, phase, target)` registers a slot or consumes an
//! early arrival. A single `dispatcher` task drains `AmStream::wait_msg()` and
//! routes each message into the matching slot.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::task::Waker;

use pluvio_ucx::worker::am::AmStream;

use crate::error::CollectiveError;

/// Header that travels alongside every collective AM payload. Encodes the
/// source rank, the protocol step / phase, and (for pipelined collectives)
/// the index of the micro-chunk within a single ring step.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AmHeader {
    pub src: u16,
    pub step: u16,
    pub phase: u8,
    pub micro_chunk: u8,
}

impl AmHeader {
    pub const BYTES: usize = 8;

    pub fn encode(self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[0..2].copy_from_slice(&self.src.to_le_bytes());
        out[2..4].copy_from_slice(&self.step.to_le_bytes());
        out[4] = self.phase;
        out[5] = self.micro_chunk;
        // bytes 6..8 reserved for future use.
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CollectiveError> {
        if bytes.len() < Self::BYTES {
            return Err(CollectiveError::BadHeader {
                got: bytes.len(),
                expected: Self::BYTES,
            });
        }
        Ok(Self {
            src: u16::from_le_bytes([bytes[0], bytes[1]]),
            step: u16::from_le_bytes([bytes[2], bytes[3]]),
            phase: bytes[4],
            micro_chunk: bytes[5],
        })
    }
}

#[derive(Hash, Eq, PartialEq, Copy, Clone, Debug)]
pub(crate) struct RecvKey {
    src: u16,
    step: u16,
    phase: u8,
    micro_chunk: u8,
}

impl From<AmHeader> for RecvKey {
    fn from(h: AmHeader) -> Self {
        Self {
            src: h.src,
            step: h.step,
            phase: h.phase,
            micro_chunk: h.micro_chunk,
        }
    }
}

/// Pending receive: dispatcher copies the bytes here when the AM arrives.
pub(crate) struct PendingRecv {
    pub target: *mut u8,
    pub len: usize,
    pub done: Rc<Cell<bool>>,
    pub waker: Cell<Option<Waker>>,
}

enum Slot {
    Pending(PendingRecv),
    /// Arrived before `recv_from` was posted. The bytes are buffered in the
    /// router until a `recv_from` consumes them.
    Early(Vec<u8>),
}

/// Demuxes the underlying `AmStream` into per-key slots.
pub struct AmRouter {
    slots: RefCell<HashMap<RecvKey, Slot>>,
}

impl AmRouter {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            slots: RefCell::new(HashMap::new()),
        })
    }

    /// Look up an early arrival for `key`. Returns `Some(bytes)` if present,
    /// otherwise `None`.
    pub(crate) fn take_early(&self, key: RecvKey) -> Option<Vec<u8>> {
        let mut slots = self.slots.borrow_mut();
        match slots.get(&key) {
            Some(Slot::Early(_)) => match slots.remove(&key) {
                Some(Slot::Early(bytes)) => Some(bytes),
                _ => None,
            },
            _ => None,
        }
    }

    /// Register a pending receive. Caller must remove this slot if its future
    /// is dropped before completion.
    pub(crate) fn register_pending(&self, key: RecvKey, pending: PendingRecv) {
        let mut slots = self.slots.borrow_mut();
        let prev = slots.insert(key, Slot::Pending(pending));
        debug_assert!(
            prev.is_none(),
            "duplicate pending registration for {:?}",
            key
        );
    }

    /// Update the waker on an existing pending entry. No-op if the slot has
    /// already been completed/removed.
    pub(crate) fn update_waker(&self, key: RecvKey, waker: Waker) {
        let slots = self.slots.borrow();
        if let Some(Slot::Pending(p)) = slots.get(&key) {
            p.waker.set(Some(waker));
        }
    }

    /// Remove a pending entry (used on cancellation). Has no effect if the
    /// dispatcher already consumed it.
    pub(crate) fn cancel_pending(&self, key: RecvKey) {
        self.slots.borrow_mut().remove(&key);
    }

    /// Called by the dispatcher when an AM arrives with payload `bytes`.
    pub(crate) fn deliver(&self, key: RecvKey, bytes: Vec<u8>) {
        let mut slots = self.slots.borrow_mut();
        match slots.remove(&key) {
            Some(Slot::Pending(p)) => {
                drop(slots);
                if bytes.len() != p.len {
                    tracing::error!(
                        "AmRouter: size mismatch for {:?}: got {}, expected {}",
                        key,
                        bytes.len(),
                        p.len
                    );
                    // Still wake the receiver so it can observe the error via
                    // a separate mechanism if needed. For phase 1+2 we just
                    // log and don't write past the target buffer.
                }
                let copy_len = bytes.len().min(p.len);
                // SAFETY: the dispatcher runs on the same thread as the
                // receiver future. If the receiver future were dropped, its
                // Drop impl would have called `cancel_pending` removing this
                // slot before yielding. Therefore p.target is still valid.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.target, copy_len);
                }
                p.done.set(true);
                if let Some(w) = p.waker.take() {
                    w.wake();
                }
            }
            None => {
                slots.insert(key, Slot::Early(bytes));
            }
            Some(Slot::Early(_)) => {
                tracing::error!("AmRouter: duplicate early arrival for {:?}", key);
            }
        }
    }

    /// Number of currently outstanding pending receives. Used in tests.
    #[allow(dead_code)]
    pub(crate) fn pending_count(&self) -> usize {
        self.slots
            .borrow()
            .values()
            .filter(|s| matches!(s, Slot::Pending(_)))
            .count()
    }
}

/// Drain the AM stream forever, routing each message into `router`. This task
/// runs alongside the user's collective futures; it terminates only when the
/// stream closes (returns `None`).
pub async fn dispatcher_loop(stream: AmStream, router: Rc<AmRouter>) {
    while let Some(mut msg) = stream.wait_msg().await {
        // SAFETY: `header()` returns a slice into the AmMsg's owned header
        // Vec; valid while `msg` is alive.
        let header = match AmHeader::decode(msg.header()) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("AmRouter dispatcher: bad header: {}", e);
                continue;
            }
        };
        let key: RecvKey = header.into();
        let mut buf = vec![0u8; msg.data_len()];
        match msg.recv_data_single(&mut buf).await {
            Ok(n) => buf.truncate(n),
            Err(e) => {
                tracing::error!("AmRouter dispatcher: recv_data_single failed: {:?}", e);
                continue;
            }
        }
        router.deliver(key, buf);
    }
}

/// A future returned by `recv_from`. Waits until the dispatcher fills the
/// target buffer.
pub(crate) struct RecvFuture {
    router: Rc<AmRouter>,
    key: RecvKey,
    done: Rc<Cell<bool>>,
    /// `true` once registered with the router. We register lazily on first
    /// poll so that early arrivals can short-circuit.
    registered: bool,
    /// Target buffer pointer/length. Required because `register_pending`
    /// captures these but we may need them again for an early-arrival copy.
    target: *mut u8,
    len: usize,
}

impl RecvFuture {
    pub(crate) fn new(router: Rc<AmRouter>, key: RecvKey, target: &mut [u8]) -> Self {
        Self {
            router,
            key,
            done: Rc::new(Cell::new(false)),
            registered: false,
            target: target.as_mut_ptr(),
            len: target.len(),
        }
    }
}

impl std::future::Future for RecvFuture {
    type Output = Result<(), CollectiveError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        if this.done.get() {
            return std::task::Poll::Ready(Ok(()));
        }
        if !this.registered {
            // Maybe the data is already buffered as an early arrival.
            if let Some(bytes) = this.router.take_early(this.key) {
                let copy_len = bytes.len().min(this.len);
                // SAFETY: `target` was constructed from a `&mut [u8]` whose
                // lifetime outlives this future via the future's borrow of it.
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), this.target, copy_len);
                }
                this.done.set(true);
                return std::task::Poll::Ready(Ok(()));
            }
            this.router.register_pending(
                this.key,
                PendingRecv {
                    target: this.target,
                    len: this.len,
                    done: this.done.clone(),
                    waker: Cell::new(Some(cx.waker().clone())),
                },
            );
            this.registered = true;
            return std::task::Poll::Pending;
        }
        // Subsequent polls: refresh waker if not yet completed.
        this.router.update_waker(this.key, cx.waker().clone());
        std::task::Poll::Pending
    }
}

impl Drop for RecvFuture {
    fn drop(&mut self) {
        if self.registered && !self.done.get() {
            self.router.cancel_pending(self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = AmHeader {
            src: 7,
            step: 3,
            phase: 1,
            micro_chunk: 9,
        };
        let bytes = h.encode();
        let decoded = AmHeader::decode(&bytes).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn header_too_short() {
        let err = AmHeader::decode(&[0u8; 4]).unwrap_err();
        match err {
            CollectiveError::BadHeader { got, expected } => {
                assert_eq!(got, 4);
                assert_eq!(expected, AmHeader::BYTES);
            }
            other => panic!("unexpected error: {:?}", other),
        }
    }

    #[test]
    fn micro_chunk_disambiguates_keys() {
        let a = AmHeader {
            src: 1,
            step: 2,
            phase: 0,
            micro_chunk: 0,
        };
        let b = AmHeader {
            src: 1,
            step: 2,
            phase: 0,
            micro_chunk: 1,
        };
        let ka: RecvKey = a.into();
        let kb: RecvKey = b.into();
        assert_ne!(ka, kb);
    }

    #[test]
    fn early_arrival_then_recv() {
        let router = AmRouter::new();
        let h = AmHeader {
            src: 1,
            step: 0,
            phase: 0,
            micro_chunk: 0,
        };
        let key: RecvKey = h.into();
        router.deliver(key, vec![1, 2, 3, 4]);
        let bytes = router.take_early(key).unwrap();
        assert_eq!(bytes, vec![1, 2, 3, 4]);
        assert!(router.take_early(key).is_none());
    }

    #[test]
    fn pending_then_deliver_copies_to_target() {
        let router = AmRouter::new();
        let h = AmHeader {
            src: 0,
            step: 5,
            phase: 1,
            micro_chunk: 0,
        };
        let key: RecvKey = h.into();

        let mut target = vec![0u8; 4];
        let done = Rc::new(Cell::new(false));
        router.register_pending(
            key,
            PendingRecv {
                target: target.as_mut_ptr(),
                len: target.len(),
                done: done.clone(),
                waker: Cell::new(None),
            },
        );
        assert_eq!(router.pending_count(), 1);
        router.deliver(key, vec![10, 20, 30, 40]);
        assert!(done.get());
        assert_eq!(target, vec![10, 20, 30, 40]);
        assert_eq!(router.pending_count(), 0);
    }
}
