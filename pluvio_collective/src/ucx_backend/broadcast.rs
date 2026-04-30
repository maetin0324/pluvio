//! Binomial-tree broadcast.
//!
//! `log2_ceil(n)` rounds: each rank that already holds the data sends to a
//! peer at distance `2^round` (relative to root). The tree fills in `O(log n)`
//! rounds with each non-root rank receiving exactly once.

use std::pin::Pin;

use futures::FutureExt;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::ucx_backend::am_router::{AmHeader, RecvFuture, RecvKey};
use crate::ucx_backend::communicator::{COLLECTIVE_AM_ID, UcxCommunicator};

const PHASE_BCAST: u8 = 5;

pub struct BroadcastFuture<'a, T> {
    inner: Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>,
    _marker: std::marker::PhantomData<&'a mut [T]>,
}

impl<'a, T> std::future::Future for BroadcastFuture<'a, T> {
    type Output = Result<(), CollectiveError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub fn broadcast_typed<'a, T>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
    root: usize,
) -> BroadcastFuture<'a, T>
where
    T: Copy + bytemuck::Pod + 'static,
{
    let fut = broadcast::<T>(comm, buf, root).boxed_local();
    BroadcastFuture {
        inner: fut,
        _marker: std::marker::PhantomData,
    }
}

async fn broadcast<'a, T>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
    root: usize,
) -> Result<(), CollectiveError>
where
    T: Copy + bytemuck::Pod + 'static,
{
    let n = comm.size();
    let rank = comm.rank();
    if n == 1 {
        return Ok(());
    }
    if root >= n {
        return Err(CollectiveError::Protocol("broadcast: root out of range"));
    }

    // Logical rank with root re-mapped to 0 (binomial tree is easiest to
    // describe rooted at logical 0).
    let logical = (rank + n - root) % n;

    // Number of rounds = ceil(log2(n))
    let rounds: u32 = (usize::BITS - (n - 1).leading_zeros()).max(1);

    let mut have_data = logical == 0;

    for round in 0..rounds {
        let mask = 1usize << round;
        // The receiver at the current round is the rank whose highest set bit
        // equals `round`; the sender is its parent (= rank with that bit
        // cleared). That exchange is set up below — no precondition state to
        // record at this point.
        let _ = mask;

        if have_data {
            // Possible sender: pair with peer at logical + mask.
            let peer_logical = logical | mask;
            if peer_logical != logical && peer_logical < n {
                let peer = (peer_logical + root) % n;
                // SAFETY: `buf` は &mut [T] だが、bcast の sender path では送るだけで
                // mutate しない。borrow は am_send().await 完了で解放される。
                let send_bytes_view: &[u8] = unsafe {
                    let ptr = buf.as_ptr() as *const u8;
                    std::slice::from_raw_parts(ptr, std::mem::size_of_val(buf))
                };
                let header = AmHeader {
                    src: rank as u16,
                    step: round as u16,
                    phase: PHASE_BCAST,
                    micro_chunk: 0,
                }
                .encode();
                let ep = comm
                    .endpoint(peer)
                    .ok_or(CollectiveError::Protocol("bcast: missing endpoint"))?
                    .clone();
                ep.am_send(
                    COLLECTIVE_AM_ID as u32,
                    &header,
                    send_bytes_view,
                    false,
                    None, // UCX に Eager/Rndv 切替を任せる
                )
                .await
                .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))?;
            }
        } else {
            // Receiver path: receive only at the round whose mask matches the
            // highest set bit of `logical`.
            let highest_bit = logical_highest_bit(logical);
            if round == highest_bit {
                let parent_logical = logical & !(1usize << round);
                let parent = (parent_logical + root) % n;
                let target_bytes = bytemuck::cast_slice_mut::<T, u8>(buf);
                let key = RecvKey::from(AmHeader {
                    src: parent as u16,
                    step: round as u16,
                    phase: PHASE_BCAST,
                    micro_chunk: 0,
                });
                RecvFuture::new(comm.router().clone(), key, target_bytes).await?;
                have_data = true;
            }
        }
    }
    Ok(())
}

#[inline]
fn logical_highest_bit(x: usize) -> u32 {
    debug_assert!(x > 0);
    (usize::BITS - 1) - x.leading_zeros()
}
