//! Bruck allgather.
//!
//! `log2(n)` rounds of pairwise exchange. After round `k`, each rank holds
//! `2^(k+1)` consecutive contributions starting at its own rank (in modular
//! rotated layout). After all rounds, a final local rotation reorders the
//! data so that `recv_buf[r * L .. (r+1) * L]` holds rank `r`'s contribution.
//!
//! For non-power-of-two `n` we use the standard Bruck variant (round k sends
//! min(remaining, half) contributions); the extra arithmetic is handled by
//! sending only the elements actually owned at each round.

use std::pin::Pin;

use futures::FutureExt;
use pluvio_ucx::async_ucx::ucp::AmProto;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::ucx_backend::am_router::{AmHeader, RecvFuture, RecvKey};
use crate::ucx_backend::communicator::{COLLECTIVE_AM_ID, UcxCommunicator};

/// Phase byte for Bruck allgather AM headers.
const PHASE_BRUCK: u8 = 4;

pub struct AllgatherFuture<'a, T> {
    inner: Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>,
    _marker: std::marker::PhantomData<&'a mut [T]>,
}

impl<'a, T> std::future::Future for AllgatherFuture<'a, T> {
    type Output = Result<(), CollectiveError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub fn bruck_allgather_typed<'a, T>(
    comm: &'a UcxCommunicator,
    send_buf: &'a [T],
    recv_buf: &'a mut [T],
) -> AllgatherFuture<'a, T>
where
    T: Copy + bytemuck::Pod + 'static,
{
    let fut = bruck_allgather::<T>(comm, send_buf, recv_buf).boxed_local();
    AllgatherFuture {
        inner: fut,
        _marker: std::marker::PhantomData,
    }
}

async fn bruck_allgather<'a, T>(
    comm: &'a UcxCommunicator,
    send_buf: &'a [T],
    recv_buf: &'a mut [T],
) -> Result<(), CollectiveError>
where
    T: Copy + bytemuck::Pod + 'static,
{
    let n = comm.size();
    let rank = comm.rank();
    let l = send_buf.len();
    if recv_buf.len() != l * n {
        return Err(CollectiveError::BadShape);
    }
    if n == 1 {
        recv_buf.copy_from_slice(send_buf);
        return Ok(());
    }

    // Bruck stores its working set at the *start* of recv_buf in modular
    // rotated layout: slot k corresponds to rank `(rank + k) % n`. We initialise
    // slot 0 with our own contribution.
    recv_buf[..l].copy_from_slice(send_buf);

    let mut count = 1usize; // contributions currently owned (in slots [0..count]).
    let mut round: u16 = 0;
    while count < n {
        let to_send = count.min(n - count);
        let send_peer = (rank + n - count) % n;
        let recv_peer = (rank + count) % n;

        // Send slots [0 .. to_send] to send_peer; receive into slots
        // [count .. count + to_send] from recv_peer.
        let send_off = 0;
        let recv_off = count;

        let send_bytes: Vec<u8> = bytemuck::cast_slice::<T, u8>(
            &recv_buf[send_off * l..(send_off + to_send) * l],
        )
        .to_vec();

        // SAFETY: send and recv refer to disjoint slot ranges of `recv_buf`
        // (`send_off=0..to_send` vs `recv_off=count..count+to_send`, and
        // `to_send <= count` so they never overlap).
        let recv_target_bytes: &mut [u8] = unsafe {
            let ptr = recv_buf[recv_off * l..(recv_off + to_send) * l].as_mut_ptr() as *mut u8;
            let byte_len = to_send * l * std::mem::size_of::<T>();
            std::slice::from_raw_parts_mut(ptr, byte_len)
        };

        let send_header = AmHeader {
            src: rank as u16,
            step: round,
            phase: PHASE_BRUCK,
            micro_chunk: 0,
        }
        .encode();
        let recv_key = RecvKey::from(AmHeader {
            src: recv_peer as u16,
            step: round,
            phase: PHASE_BRUCK,
            micro_chunk: 0,
        });

        let send_ep = comm
            .endpoint(send_peer)
            .ok_or(CollectiveError::Protocol("bruck: missing endpoint"))?
            .clone();
        let send_fut = async move {
            send_ep
                .am_send(
                    COLLECTIVE_AM_ID as u32,
                    &send_header,
                    &send_bytes,
                    false,
                    Some(AmProto::Eager),
                )
                .await
                .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))
        };
        let recv_fut = RecvFuture::new(comm.router().clone(), recv_key, recv_target_bytes);
        let (sr, rr) = futures::future::join(send_fut, recv_fut).await;
        sr?;
        rr?;

        count += to_send;
        round += 1;
    }

    // Final local rotation: slot k currently holds rank `(rank + k) % n`'s
    // contribution. We want slot `(rank + k) % n` (the global rank index) to
    // hold that data. So shift right by `rank` slots (mod n) into a temp
    // buffer, then copy back.
    let mut rotated: Vec<T> = vec![send_buf[0]; recv_buf.len()];
    for k in 0..n {
        let global = (rank + k) % n;
        rotated[global * l..(global + 1) * l].copy_from_slice(&recv_buf[k * l..(k + 1) * l]);
    }
    recv_buf.copy_from_slice(&rotated);
    Ok(())
}
