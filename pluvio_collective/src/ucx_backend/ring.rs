//! Ring allreduce on top of UCX **tag-matched** messaging.
//!
//! Two phases of `n - 1` steps each (n = communicator size):
//! - reduce-scatter: each rank ends up owning the reduced value of one chunk.
//! - allgather: that chunk is forwarded around the ring.
//!
//! Send and receive of each step run concurrently via `futures::join`.
//!
//! 旧 AM-router 経路 (`am_send` + `RecvFuture` + HashMap dispatch) を廃止し、
//! UCX のタグマッチング (`tag_send` / `tag_recv`) に直接乗せる:
//!   - 受信側が事前に buffer を post できるため zero-copy RDMA put/get に乗りやすい
//!   - HW tag matching (Mellanox CX-5+) の恩恵
//!   - per-message router HashMap dispatch のオーバーヘッドが消える
//!
//! 64-bit タグの組み立ては `super::tag::encode_tag` 参照。

use std::mem::MaybeUninit;
use std::pin::Pin;
use std::rc::Rc;

use futures::FutureExt;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::communicator::UcxCommunicator;
use crate::ucx_backend::tag::encode_tag;

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
        // SAFETY: send borrows `buf[send_off..send_off+chunk]` immutably while
        // recv writes into the disjoint `tmp` buffer. The borrow ends with
        // `join().await` — the local reduce on `buf[recv_idx]` runs after.
        let send_bytes_view: &[u8] = unsafe {
            let ptr = buf[send_off..send_off + chunk].as_ptr() as *const u8;
            std::slice::from_raw_parts(ptr, chunk * std::mem::size_of::<T>())
        };

        let send_tag = encode_tag(rank, step as u16, /*phase=*/ 0, 0);
        let recv_tag = encode_tag(prev, step as u16, /*phase=*/ 0, 0);

        let recv_target_bytes = bytemuck::cast_slice_mut::<T, u8>(tmp.as_mut_slice());

        run_send_recv(
            &next_ep,
            comm,
            send_bytes_view,
            send_tag,
            recv_tag,
            recv_target_bytes,
        )
        .await?;

        // Local reduce: buf[recv_idx] = O(buf[recv_idx], tmp)  (SIMD-friendly)
        let recv_off = recv_idx * chunk;
        let dst = &mut buf[recv_off..recv_off + chunk];
        O::reduce_in_place(dst, &tmp);
    }

    // Phase 2: allgather
    for step in 0..(n - 1) {
        let send_idx = (rank + n - step + 1) % n;
        let recv_idx = (rank + n - step) % n;

        let send_off = send_idx * chunk;
        let recv_off = recv_idx * chunk;
        // SAFETY: send and recv refer to disjoint chunks of `buf` (they cover
        // different `recv_idx` and `send_idx` slices since `n >= 2`).
        let send_bytes_view: &[u8] = unsafe {
            let ptr = buf[send_off..send_off + chunk].as_ptr() as *const u8;
            std::slice::from_raw_parts(ptr, chunk * std::mem::size_of::<T>())
        };
        let recv_target_bytes: &mut [u8] = unsafe {
            let ptr = buf[recv_off..recv_off + chunk].as_mut_ptr() as *mut u8;
            let len = chunk * std::mem::size_of::<T>();
            std::slice::from_raw_parts_mut(ptr, len)
        };

        let send_tag = encode_tag(rank, step as u16, /*phase=*/ 1, 0);
        let recv_tag = encode_tag(prev, step as u16, /*phase=*/ 1, 0);

        run_send_recv(
            &next_ep,
            comm,
            send_bytes_view,
            send_tag,
            recv_tag,
            recv_target_bytes,
        )
        .await?;
    }

    Ok(())
}

/// `&mut [u8]` を `&mut [MaybeUninit<u8>]` として再借用 (reborrow)。
/// 関数境界での mutable reborrow なので、呼び出し側の `&mut [u8]` は
/// 戻り値の lifetime 内で使えなくなる (=aliasing UB を回避)。
#[inline]
fn as_uninit_mut(s: &mut [u8]) -> &mut [MaybeUninit<u8>] {
    let len = s.len();
    let ptr = s.as_mut_ptr() as *mut MaybeUninit<u8>;
    // SAFETY: `MaybeUninit<u8>` は `u8` と同じ表現を持ち、aliasing 制約も同じ。
    // 元の `s: &mut [u8]` は consume され、戻り値 `&mut [MaybeUninit<u8>]` のみが
    // 当該領域に対する exclusive borrow となる。
    unsafe { std::slice::from_raw_parts_mut(ptr, len) }
}

async fn run_send_recv<'a>(
    next_ep: &Rc<pluvio_ucx::worker::endpoint::Endpoint>,
    comm: &'a UcxCommunicator,
    send_bytes: &'a [u8],
    send_tag: u64,
    recv_tag: u64,
    recv_target: &'a mut [u8],
) -> Result<(), CollectiveError> {
    // pluvio_ucx::Endpoint::tag_send / Worker::tag_recv は内部で activate/deactivate
    // を呼ぶが、両者を `join` で並走すると先に完了した方が deactivate を打ち、
    // reactor の active counter が一時的に 0 になる race がある (state 遷移ベース、
    // 操作カウントではない)。これにより未完了の recv 側で polling が止まり SIGSEGV
    // になる。回避策: 並走範囲を 1 回の activate でラップし、内部 async-ucx API
    // (`endpoint.endpoint.tag_send`, `worker.inner().tag_recv`) を直接呼ぶ。
    let worker = comm.worker();
    worker.activate();

    let send_fut = async {
        next_ep
            .endpoint
            .tag_send(send_tag, send_bytes)
            .await
            .map(|_| ())
            .map_err(|e| CollectiveError::Ucx(format!("tag_send: {:?}", e)))
    };
    let recv_target_uninit = as_uninit_mut(recv_target);
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
    let (send_res, recv_res) = join_res;
    send_res?;
    recv_res?;
    Ok(())
}
