//! Ring allreduce on top of `pluvio_ucx` Active Messages.
//!
//! Two phases of `n - 1` steps each (n = communicator size):
//! - reduce-scatter: each rank ends up owning the reduced value of one chunk.
//! - allgather: that chunk is forwarded around the ring.
//!
//! Send and receive of each step run concurrently via `futures::join`.

use std::pin::Pin;
use std::rc::Rc;

use futures::FutureExt;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::am_router::{AmHeader, RecvFuture, RecvKey};
use crate::ucx_backend::communicator::{COLLECTIVE_AM_ID, UcxCommunicator};

/// Boxed allreduce future. The body is non-trivial (multiple await points) so
/// we erase the concrete `async` block type behind a Box for the GAT.
pub struct RingAllreduceFuture<'a, T> {
    inner: Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>,
    _marker: std::marker::PhantomData<&'a mut [T]>,
}

impl<'a, T> std::future::Future for RingAllreduceFuture<'a, T> {
    type Output = Result<(), CollectiveError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub fn ring_allreduce_typed<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
) -> RingAllreduceFuture<'a, T>
where
    T: Copy + Default + bytemuck::Pod + 'static,
    O: Op<T>,
{
    let fut = ring_allreduce::<T, O>(comm, buf).boxed_local();
    RingAllreduceFuture {
        inner: fut,
        _marker: std::marker::PhantomData,
    }
}

async fn ring_allreduce<'a, T, O>(
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
    if !buf.len().is_multiple_of(n) {
        return Err(CollectiveError::BadShape);
    }
    let chunk = buf.len() / n;
    let next = (rank + 1) % n;
    let prev = (rank + n - 1) % n;

    let next_ep = comm
        .endpoint(next)
        .ok_or(CollectiveError::Protocol("no endpoint to next rank"))?
        .clone();

    let mut tmp: Vec<T> = vec![T::default(); chunk];

    // Phase 1: reduce-scatter
    for step in 0..(n - 1) {
        let send_idx = (rank + n - step) % n;
        let recv_idx = (rank + n - step - 1) % n;

        let send_off = send_idx * chunk;
        let send_bytes = bytemuck::cast_slice::<T, u8>(&buf[send_off..send_off + chunk]).to_vec();

        let key = RecvKey::from(AmHeader {
            src: prev as u16,
            step: step as u16,
            phase: 0,
            micro_chunk: 0,
        });
        let recv_target_bytes = bytemuck::cast_slice_mut::<T, u8>(tmp.as_mut_slice());

        run_send_recv(
            &next_ep,
            comm,
            send_bytes,
            AmHeader {
                src: rank as u16,
                step: step as u16,
                phase: 0,
                micro_chunk: 0,
            },
            key,
            recv_target_bytes,
        )
        .await?;

        // Local reduce: buf[recv_idx] = O(buf[recv_idx], tmp)
        let recv_off = recv_idx * chunk;
        for i in 0..chunk {
            buf[recv_off + i] = O::apply(buf[recv_off + i], tmp[i]);
        }
    }

    // Phase 2: allgather
    for step in 0..(n - 1) {
        let send_idx = (rank + n - step + 1) % n;
        let recv_idx = (rank + n - step) % n;

        let send_off = send_idx * chunk;
        let send_bytes = bytemuck::cast_slice::<T, u8>(&buf[send_off..send_off + chunk]).to_vec();

        let recv_off = recv_idx * chunk;
        // SAFETY: send and recv refer to disjoint chunks of `buf` (they cover
        // different `recv_idx` and `send_idx` slices since `n >= 2`).
        // Constructing a separate &mut [u8] from raw pointers avoids the
        // borrow checker complaining about simultaneously borrowing two
        // disjoint slices of `buf`.
        let recv_target_bytes: &mut [u8] = unsafe {
            let ptr = buf[recv_off..recv_off + chunk].as_mut_ptr() as *mut u8;
            let len = chunk * std::mem::size_of::<T>();
            std::slice::from_raw_parts_mut(ptr, len)
        };

        run_send_recv(
            &next_ep,
            comm,
            send_bytes,
            AmHeader {
                src: rank as u16,
                step: step as u16,
                phase: 1,
                micro_chunk: 0,
            },
            RecvKey::from(AmHeader {
                src: prev as u16,
                step: step as u16,
                phase: 1,
                micro_chunk: 0,
            }),
            recv_target_bytes,
        )
        .await?;
    }

    Ok(())
}

async fn run_send_recv<'a>(
    next_ep: &Rc<pluvio_ucx::worker::endpoint::Endpoint>,
    comm: &'a UcxCommunicator,
    send_bytes: Vec<u8>,
    send_header: AmHeader,
    recv_key: RecvKey,
    recv_target: &'a mut [u8],
) -> Result<(), CollectiveError> {
    let header_bytes = send_header.encode();

    let send_fut = async move {
        next_ep
            .am_send(
                COLLECTIVE_AM_ID as u32,
                &header_bytes,
                &send_bytes,
                false,
                // proto: None で UCX に任せる。AmProto::Eager 強制だと大 msg で
                // 内部 fragment 連送になり、256 KiB が 30 ms 超になる。
                None,
            )
            .await
            .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))
    };

    let recv_fut = RecvFuture::new(comm.router().clone(), recv_key, recv_target);

    let (send_res, recv_res) = futures::future::join(send_fut, recv_fut).await;
    send_res?;
    recv_res?;
    Ok(())
}
