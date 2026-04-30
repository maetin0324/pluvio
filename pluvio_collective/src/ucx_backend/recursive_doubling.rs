//! Recursive-doubling allreduce.
//!
//! For `n = 2^k` ranks: `log2(n)` rounds of pairwise exchange, each rank
//! exchanges with the peer at distance `2^r` and locally reduces the result
//! into `buf`. After `log2(n)` rounds every rank holds the same fully-reduced
//! buffer.
//!
//! For non-power-of-two `n` we fall back to the plain ring path because the
//! Rabenseifner-style reduction needed to handle the residue is out of scope
//! for this phase.

use std::pin::Pin;

use futures::FutureExt;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::am_router::{AmHeader, RecvFuture, RecvKey};
use crate::ucx_backend::communicator::{COLLECTIVE_AM_ID, UcxCommunicator};
use crate::ucx_backend::ring::ring_allreduce_typed;

/// Phase byte used by the recursive-doubling AM headers; distinct from the
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

        let send_bytes: Vec<u8> = bytemuck::cast_slice::<T, u8>(buf).to_vec();
        let recv_target_bytes: &mut [u8] = bytemuck::cast_slice_mut::<T, u8>(&mut tmp);

        let send_header = AmHeader {
            src: rank as u16,
            step: r as u16,
            phase: PHASE_RD,
            micro_chunk: 0,
        }
        .encode();
        let recv_key = RecvKey::from(AmHeader {
            src: peer as u16,
            step: r as u16,
            phase: PHASE_RD,
            micro_chunk: 0,
        });

        let send_fut = async move {
            peer_ep
                .am_send(
                    COLLECTIVE_AM_ID as u32,
                    &send_header,
                    &send_bytes,
                    false,
                    // proto: None — UCX_RNDV_THRESH に基づく自動選択。
                    None,
                )
                .await
                .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))
        };
        let recv_fut = RecvFuture::new(comm.router().clone(), recv_key, recv_target_bytes);
        let (sr, rr) = futures::future::join(send_fut, recv_fut).await;
        sr?;
        rr?;

        // Local reduce into `buf`.
        for (d, s) in buf.iter_mut().zip(tmp.iter()) {
            *d = O::apply(*d, *s);
        }
    }
    Ok(())
}
