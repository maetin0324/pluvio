//! Pipelined ring allreduce.
//!
//! Each ring step's chunk is split into `M = config.micro_chunks` micro-chunks.
//! All `M` micro-chunk chains run concurrently — at any moment up to
//! `config.max_in_flight` chains are scheduled, the rest are awaiting
//! backpressure from `FuturesUnordered`.
//!
//! Within a single micro-chunk chain, the `n-1` reduce-scatter steps still
//! run sequentially (each step's send depends on the previous step's reduce);
//! the parallelism is across micro-chunks. This matches the libNBC pipelining
//! strategy and gives the dispatcher a steady stream of work.

use std::pin::Pin;
use std::rc::Rc;

use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};
use pluvio_ucx::async_ucx::ucp::AmProto;
use pluvio_ucx::worker::endpoint::Endpoint;

use crate::Communicator;
use crate::error::CollectiveError;
use crate::op::Op;
use crate::ucx_backend::am_router::{AmHeader, AmRouter, RecvFuture, RecvKey};
use crate::ucx_backend::communicator::{COLLECTIVE_AM_ID, UcxCommunicator};

/// Configuration for pipelined ring allreduce.
#[derive(Copy, Clone, Debug)]
pub struct PipelineConfig {
    /// Number of micro-chunks per ring step. Must divide `buf.len() / size()`
    /// and be in `1..=255` (the AM header micro_chunk byte fits in a `u8`).
    pub micro_chunks: usize,
    /// Maximum chains pumped concurrently into `FuturesUnordered`. Acts as a
    /// backpressure knob — too low under-pipelines, too high stresses the AM
    /// router and UCX request pool.
    pub max_in_flight: usize,
    /// AM protocol hint passed to UCX. `None` で UCX に Eager/Rndv 切替を任せる
    /// (UCX_RNDV_THRESH 経由)。`Some(AmProto::Eager)` を強制すると ~64 KiB を
    /// 越えた micro_chunk で UCX が MTU サイズの fragment 連送に分解し、各
    /// fragment で ack 待ちが入るため極端に遅くなる (256 KiB allreduce が
    /// ~30 ms / 287×)。
    pub proto: Option<AmProto>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            micro_chunks: 4,
            max_in_flight: 8,
            proto: None,
        }
    }
}

/// Boxed pipelined-ring future. Same erasure pattern as `RingAllreduceFuture`.
pub struct PipelinedRingAllreduceFuture<'a, T> {
    inner: Pin<Box<dyn std::future::Future<Output = Result<(), CollectiveError>> + 'a>>,
    _marker: std::marker::PhantomData<&'a mut [T]>,
}

impl<'a, T> std::future::Future for PipelinedRingAllreduceFuture<'a, T> {
    type Output = Result<(), CollectiveError>;
    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

pub fn pipelined_ring_allreduce_typed<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
    config: PipelineConfig,
) -> PipelinedRingAllreduceFuture<'a, T>
where
    T: Copy + Default + bytemuck::Pod + 'static,
    O: Op<T>,
{
    let fut = pipelined_ring::<T, O>(comm, buf, config).boxed_local();
    PipelinedRingAllreduceFuture {
        inner: fut,
        _marker: std::marker::PhantomData,
    }
}

/// Plain pointer + length pair to one micro-chunk slot inside `buf`. We use
/// raw pointers so each per-chain async block can own its own slot list
/// without fighting the borrow checker over a single `&mut [T]`. Safety is
/// enforced by the surrounding `pipelined_ring` (single-threaded executor +
/// disjoint slot indexing).
#[derive(Copy, Clone)]
struct Slot<T> {
    ptr: *mut T,
    len: usize,
}

unsafe impl<T> Send for Slot<T> {}

impl<T> Slot<T> {
    /// SAFETY: caller must ensure no other slot pointing to overlapping
    /// memory is used concurrently.
    #[allow(clippy::wrong_self_convention)]
    unsafe fn as_slice(self) -> &'static [T] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    /// SAFETY: as above; this `&mut` must be the unique active reference.
    #[allow(clippy::wrong_self_convention)]
    unsafe fn as_slice_mut(self) -> &'static mut [T] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

async fn pipelined_ring<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
    config: PipelineConfig,
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
    if config.micro_chunks == 0 || config.max_in_flight == 0 {
        return Err(CollectiveError::Protocol(
            "pipelined ring: micro_chunks and max_in_flight must be > 0",
        ));
    }
    if config.micro_chunks > u8::MAX as usize {
        return Err(CollectiveError::Protocol(
            "pipelined ring: micro_chunks > 255 (does not fit in AmHeader::micro_chunk)",
        ));
    }
    if !buf.len().is_multiple_of(n) {
        return Err(CollectiveError::BadShape);
    }
    let chunk = buf.len() / n;
    if !chunk.is_multiple_of(config.micro_chunks) {
        return Err(CollectiveError::BadShape);
    }
    let micro_size = chunk / config.micro_chunks;
    let next = (rank + 1) % n;
    let prev = (rank + n - 1) % n;
    let next_ep = comm
        .endpoint(next)
        .ok_or(CollectiveError::Protocol("no endpoint to next rank"))?
        .clone();

    // Build a flat Slot table indexed by `chunk_idx * micro_chunks + m`.
    let total_slots = n * config.micro_chunks;
    let buf_base: *mut T = buf.as_mut_ptr();
    let slots: Vec<Slot<T>> = (0..total_slots)
        .map(|i| {
            let chunk_idx = i / config.micro_chunks;
            let m = i % config.micro_chunks;
            let off = chunk_idx * chunk + m * micro_size;
            // SAFETY: off + micro_size <= n * chunk = buf.len().
            Slot {
                ptr: unsafe { buf_base.add(off) },
                len: micro_size,
            }
        })
        .collect();

    // Per-chain scratch buffer for receiving inbound bytes during
    // reduce-scatter. One Box per micro-chunk so chains never alias.
    let mut tmp_storage: Vec<Vec<T>> = (0..config.micro_chunks)
        .map(|_| vec![T::default(); micro_size])
        .collect();
    let tmp_slots: Vec<Slot<T>> = tmp_storage
        .iter_mut()
        .map(|v| Slot {
            ptr: v.as_mut_ptr(),
            len: v.len(),
        })
        .collect();


    let router = comm.router().clone();
    let proto = config.proto;

    // === Phase 1: reduce-scatter ===
    {
        let mut chains: FuturesUnordered<_> = FuturesUnordered::new();
        let mut launched = 0usize;
        for (m, tmp_slot) in tmp_slots.iter().enumerate() {
            launched += 1;
            chains.push(reduce_scatter_chain::<T, O>(
                next_ep.clone(),
                router.clone(),
                slots.clone(),
                config.micro_chunks,
                m,
                *tmp_slot,
                rank,
                prev,
                n,
                proto,
            ));
            if launched >= config.max_in_flight
                && let Some(res) = chains.next().await
            {
                res?;
            }
        }
        while let Some(res) = chains.next().await {
            res?;
        }
    }

    // === Phase 2: allgather ===
    {
        let mut chains: FuturesUnordered<_> = FuturesUnordered::new();
        let mut launched = 0usize;
        for m in 0..config.micro_chunks {
            launched += 1;
            chains.push(allgather_chain::<T>(
                next_ep.clone(),
                router.clone(),
                slots.clone(),
                config.micro_chunks,
                m,
                rank,
                prev,
                n,
                proto,
            ));
            if launched >= config.max_in_flight
                && let Some(res) = chains.next().await
            {
                res?;
            }
        }
        while let Some(res) = chains.next().await {
            res?;
        }
    }

    // Keep `tmp_storage` alive through both phases.
    drop(tmp_storage);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn reduce_scatter_chain<T, O>(
    next_ep: Rc<Endpoint>,
    router: Rc<AmRouter>,
    slots: Vec<Slot<T>>,
    micro_chunks: usize,
    m: usize,
    tmp_slot: Slot<T>,
    rank: usize,
    prev: usize,
    n: usize,
    proto: Option<AmProto>,
) -> Result<(), CollectiveError>
where
    T: Copy + Default + bytemuck::Pod + 'static,
    O: Op<T>,
{
    for step in 0..(n - 1) {
        let send_idx = (rank + n - step) % n;
        let recv_idx = (rank + n - step - 1) % n;
        let send_slot = slots[send_idx * micro_chunks + m];
        let recv_slot = slots[recv_idx * micro_chunks + m];

        // SAFETY: each chain writes into a unique micro_chunk row of `slots`.
        // For phase 1 the only mutation is the local reduce on `recv_slot`
        // which is exclusive to micro-chunk `m`. Concurrent chains touch
        // different micro_chunks, never the same memory.
        let send_bytes: Vec<u8> =
            bytemuck::cast_slice::<T, u8>(unsafe { send_slot.as_slice() }).to_vec();
        let recv_target_bytes: &mut [u8] =
            bytemuck::cast_slice_mut::<T, u8>(unsafe { tmp_slot.as_slice_mut() });

        let send_header = AmHeader {
            src: rank as u16,
            step: step as u16,
            phase: 0,
            micro_chunk: m as u8,
        }
        .encode();
        let recv_key = RecvKey::from(AmHeader {
            src: prev as u16,
            step: step as u16,
            phase: 0,
            micro_chunk: m as u8,
        });

        let send_fut = {
            let next_ep = next_ep.clone();
            async move {
                next_ep
                    .am_send(
                        COLLECTIVE_AM_ID as u32,
                        &send_header,
                        &send_bytes,
                        false,
                        proto,
                    )
                    .await
                    .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))
            }
        };
        let recv_fut = RecvFuture::new(router.clone(), recv_key, recv_target_bytes);
        let (sr, rr) = futures::future::join(send_fut, recv_fut).await;
        sr?;
        rr?;

        // Local reduce.
        // SAFETY: `recv_slot` is unique to this chain in phase 1.
        let dst = unsafe { recv_slot.as_slice_mut() };
        let src = unsafe { tmp_slot.as_slice() };
        for (d, s) in dst.iter_mut().zip(src.iter()) {
            *d = O::apply(*d, *s);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn allgather_chain<T>(
    next_ep: Rc<Endpoint>,
    router: Rc<AmRouter>,
    slots: Vec<Slot<T>>,
    micro_chunks: usize,
    m: usize,
    rank: usize,
    prev: usize,
    n: usize,
    proto: Option<AmProto>,
) -> Result<(), CollectiveError>
where
    T: Copy + bytemuck::Pod + 'static,
{
    for step in 0..(n - 1) {
        let send_idx = (rank + n - step + 1) % n;
        let recv_idx = (rank + n - step) % n;
        let send_slot = slots[send_idx * micro_chunks + m];
        let recv_slot = slots[recv_idx * micro_chunks + m];

        // SAFETY: as above — chain `m` is the sole writer for the
        // `*_slot` rows it touches in phase 2.
        let send_bytes: Vec<u8> =
            bytemuck::cast_slice::<T, u8>(unsafe { send_slot.as_slice() }).to_vec();
        let recv_target_bytes: &mut [u8] =
            bytemuck::cast_slice_mut::<T, u8>(unsafe { recv_slot.as_slice_mut() });

        let send_header = AmHeader {
            src: rank as u16,
            step: step as u16,
            phase: 1,
            micro_chunk: m as u8,
        }
        .encode();
        let recv_key = RecvKey::from(AmHeader {
            src: prev as u16,
            step: step as u16,
            phase: 1,
            micro_chunk: m as u8,
        });

        let send_fut = {
            let next_ep = next_ep.clone();
            async move {
                next_ep
                    .am_send(
                        COLLECTIVE_AM_ID as u32,
                        &send_header,
                        &send_bytes,
                        false,
                        proto,
                    )
                    .await
                    .map_err(|e| CollectiveError::Ucx(format!("am_send: {:?}", e)))
            }
        };
        let recv_fut = RecvFuture::new(router.clone(), recv_key, recv_target_bytes);
        let (sr, rr) = futures::future::join(send_fut, recv_fut).await;
        sr?;
        rr?;
    }
    Ok(())
}
