//! Scatter on top of `pluvio_ucx` Active Messages.
//!
//! At rank `root`, the input is split into `size` equally-sized chunks; chunk
//! `r` is sent to rank `r` (with the local chunk copied directly into the
//! receive buffer). Non-root ranks post a single receive from `root`.

use std::pin::Pin;

use futures::FutureExt;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::ucx_backend::am_router::{AmHeader, RecvFuture, RecvKey};
use crate::ucx_backend::communicator::{COLLECTIVE_AM_ID, UcxCommunicator};

/// Boxed scatter future. Same erasure pattern as `RingAllreduceFuture`.
pub struct ScatterFuture<'a, T> {
    inner: Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>,
    _marker: std::marker::PhantomData<&'a mut [T]>,
}

impl<'a, T> std::future::Future for ScatterFuture<'a, T> {
    type Output = Result<(), CollectiveError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub fn scatter_typed<'a, T>(
    comm: &'a UcxCommunicator,
    send_buf: Option<&'a [T]>,
    recv_buf: &'a mut [T],
    root: usize,
) -> ScatterFuture<'a, T>
where
    T: Copy + bytemuck::Pod + 'static,
{
    let fut = scatter::<T>(comm, send_buf, recv_buf, root).boxed_local();
    ScatterFuture {
        inner: fut,
        _marker: std::marker::PhantomData,
    }
}

/// Phase byte used in the AM header to tag scatter traffic. Distinct from the
/// ring-allreduce phases (0 = reduce-scatter, 1 = allgather).
pub(crate) const PHASE_SCATTER: u8 = 2;

async fn scatter<'a, T>(
    comm: &'a UcxCommunicator,
    send_buf: Option<&'a [T]>,
    recv_buf: &'a mut [T],
    root: usize,
) -> Result<(), CollectiveError>
where
    T: Copy + bytemuck::Pod + 'static,
{
    let n = comm.size();
    let rank = comm.rank();
    if root >= n {
        return Err(CollectiveError::Protocol("scatter: root out of range"));
    }
    if n == 1 {
        let sb = send_buf.ok_or(CollectiveError::Protocol(
            "scatter: send_buf required at root",
        ))?;
        if sb.len() != recv_buf.len() {
            return Err(CollectiveError::BadShape);
        }
        recv_buf.copy_from_slice(sb);
        return Ok(());
    }

    let chunk = recv_buf.len();

    if rank == root {
        let sb = send_buf.ok_or(CollectiveError::Protocol(
            "scatter: send_buf required at root",
        ))?;
        if sb.len() != chunk * n {
            return Err(CollectiveError::BadShape);
        }
        // Local copy for the root's own chunk.
        let off = rank * chunk;
        recv_buf.copy_from_slice(&sb[off..off + chunk]);

        // Send each peer their chunk. Run sends concurrently so the wire is
        // kept busy across all endpoints.
        let header_bytes = AmHeader {
            src: root as u16,
            step: root as u16,
            phase: PHASE_SCATTER,
            micro_chunk: 0,
        }
        .encode();

        let mut sends = Vec::with_capacity(n - 1);
        for r in 0..n {
            if r == root {
                continue;
            }
            // SAFETY: `sb` (`send_buf` at root) は &[T] の borrow なので、
            // 各 chunk の view を `&[u8]` として扱える。borrow は join_all().await
            // 完了で解放される (戻る前に sb は使い続けるだけで mutate しない)。
            let payload_view: &[u8] = unsafe {
                let s = &sb[r * chunk..(r + 1) * chunk];
                let ptr = s.as_ptr() as *const u8;
                std::slice::from_raw_parts(ptr, std::mem::size_of_val(s))
            };
            let ep = comm
                .endpoint(r)
                .ok_or(CollectiveError::Protocol("scatter: missing endpoint"))?
                .clone();
            let header_bytes_ref = &header_bytes;
            sends.push(async move {
                ep.am_send(
                    COLLECTIVE_AM_ID as u32,
                    header_bytes_ref,
                    payload_view,
                    false,
                    None, // UCX に Eager/Rndv 切替を任せる
                )
                .await
                .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))
            });
        }
        for res in futures::future::join_all(sends).await {
            res?;
        }
        Ok(())
    } else {
        // Receive a single message from root.
        let key = RecvKey::from(AmHeader {
            src: root as u16,
            step: root as u16,
            phase: PHASE_SCATTER,
            micro_chunk: 0,
        });
        let target_bytes = bytemuck::cast_slice_mut::<T, u8>(recv_buf);
        RecvFuture::new(comm.router().clone(), key, target_bytes).await
    }
}
