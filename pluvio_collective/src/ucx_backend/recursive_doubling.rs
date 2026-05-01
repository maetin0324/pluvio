//! Recursive-doubling allreduce on UCX **tag-matched** messaging.
//!
//! For `n = 2^k` ranks: `log2(n)` rounds of pairwise exchange, each rank
//! exchanges with the peer at distance `2^r` and locally reduces the result
//! into `buf`. After `log2(n)` rounds every rank holds the same fully-reduced
//! buffer.
//!
//! For non-power-of-two `n` we fall back to the plain ring path because the
//! Rabenseifner-style reduction needed to handle the residue is out of scope
//! for this phase.

use std::mem::MaybeUninit;
use std::pin::Pin;

use futures::FutureExt;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::communicator::UcxCommunicator;
use crate::ucx_backend::ring::ring_allreduce_typed;
use crate::ucx_backend::tag::encode_tag;

/// Phase byte used by the recursive-doubling tag headers; distinct from the
/// ring (0/1) and scatter (2) phases.
const PHASE_RD: u8 = 3;

/// Boxed RD future, same erasure pattern as the ring future.
pub struct RecursiveDoublingFuture<'a, T> {
    inner: Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>,
    _marker: std::marker::PhantomData<&'a mut [T]>,
}

impl<'a, T> std::future::Future for RecursiveDoublingFuture<'a, T> {
    type Output = Result<(), CollectiveError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub fn recursive_doubling_typed<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
) -> RecursiveDoublingFuture<'a, T>
where
    T: Copy + Default + bytemuck::Pod + 'static,
    O: Op<T>,
{
    let fut = recursive_doubling::<T, O>(comm, buf).boxed_local();
    RecursiveDoublingFuture {
        inner: fut,
        _marker: std::marker::PhantomData,
    }
}

async fn recursive_doubling<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
) -> Result<(), CollectiveError>
where
    T: Copy + Default + bytemuck::Pod + 'static,
    O: Op<T>,
{
    let n = comm.size();
    let rank = comm.rank();
    if n <= 1 {
        return Ok(());
    }
    if !n.is_power_of_two() {
        // Fallback: ring path handles arbitrary n. Documented behaviour.
        return ring_allreduce_typed::<T, O>(comm, buf).await;
    }

    let rounds = n.trailing_zeros() as usize; // log2(n)
    let mut tmp: Vec<T> = vec![T::default(); buf.len()];

    for r in 0..rounds {
        let peer = rank ^ (1usize << r);
        let peer_ep = comm
            .endpoint(peer)
            .ok_or(CollectiveError::Protocol("rd: missing endpoint to peer"))?
            .clone();

        // SAFETY: send borrows `buf` immutably while recv writes `tmp` (disjoint).
        // 借用は join().await の終了で解放され、その後の reduce で `buf` を mut 借用する。
        let send_bytes_view: &[u8] = unsafe {
            let ptr = buf.as_ptr() as *const u8;
            std::slice::from_raw_parts(ptr, std::mem::size_of_val(buf))
        };
        // `tmp` を直接 MaybeUninit ビューに reborrow し、二重借用を避ける。
        // SAFETY: `MaybeUninit<u8>` は `u8` 同等の repr。`tmp: &mut Vec<T>` は
        // この slice 借用が活きている間使われない。
        let recv_target_uninit: &mut [MaybeUninit<u8>] = unsafe {
            let p = tmp.as_mut_ptr() as *mut MaybeUninit<u8>;
            let len = std::mem::size_of_val::<[T]>(&tmp[..]);
            std::slice::from_raw_parts_mut(p, len)
        };

        let send_tag = encode_tag(rank, r as u16, PHASE_RD, 0);
        let recv_tag = encode_tag(peer, r as u16, PHASE_RD, 0);

        // ring.rs と同じ理由で、join 全体を 1 回の activate でラップする。
        let worker = comm.worker();
        worker.activate();
        let send_fut = async {
            peer_ep
                .endpoint
                .tag_send(send_tag, send_bytes_view)
                .await
                .map(|_| ())
                .map_err(|e| CollectiveError::Ucx(format!("tag_send: {:?}", e)))
        };
        let recv_fut = async {
            worker
                .inner()
                .tag_recv(recv_tag, recv_target_uninit)
                .await
                .map(|_| ())
                .map_err(|e| CollectiveError::Ucx(format!("tag_recv: {:?}", e)))
        };
        let join_res = futures::future::join(send_fut, recv_fut).await;
        worker.deactivate();
        let (sr, rr) = join_res;
        sr?;
        rr?;

        // Local reduce into `buf` (SIMD-friendly bulk path).
        O::reduce_in_place(buf, &tmp);
    }
    Ok(())
}
