use std::rc::Rc;
use std::time::Instant;

use tracing::instrument;

use crate::stats::is_stats_enabled;
use crate::worker::{endpoint::Endpoint, Worker};

pub type AmStreamInner = async_ucx::ucp::AmStream;

#[derive(Clone)]
pub struct AmStream {
    stream: AmStreamInner,
    worker: Rc<Worker>,
    id: u16,
}

impl Worker {
    #[instrument(level = "trace", skip(self))]
    pub fn am_stream(self: &Rc<Self>, id: u16) -> Result<AmStream, async_ucx::Error> {
        let res = self.worker.am_stream(id);

        match res {
            Ok(stream) => {
                self.listener_ids.borrow_mut().insert(id.to_string());
                Ok(AmStream {
                    stream,
                    worker: self.clone(),
                    id,
                })
            }
            Err(e) => Err(e),
        }
    }
}

impl Endpoint {
    #[instrument(level = "trace", skip(self, header, data))]
    #[async_backtrace::framed]
    pub async fn am_send(
        &self,
        id: u32,
        header: &[u8],
        data: &[u8],
        need_reply: bool,
        proto: Option<async_ucx::ucp::AmProto>,
    ) -> Result<(), async_ucx::Error> {
        let start = if is_stats_enabled() { Some(Instant::now()) } else { None };
        self.worker.activate();

        let ret = self
            .endpoint
            .am_send(id, header, data, need_reply, proto)
            .await;

        if let Some(start) = start {
            let total_us = start.elapsed().as_micros() as u64;
            tracing::debug!(
                "AM_SEND_TIMING op=\"am_send\" am_id={} header_bytes={} data_bytes={} need_reply={} total_us={}",
                id, header.len(), data.len(), need_reply, total_us
            );
        }

        // Do not deactivate here - messages may still be in flight in UCX layer
        // Let the reactor manage worker state based on actual activity
        ret
    }

    #[instrument(level = "trace", skip(self, header, iov))]
    #[async_backtrace::framed]
    pub async fn am_send_vectorized(
        &self,
        id: u32,
        header: &[u8],
        iov: &[std::io::IoSlice<'_>],
        need_reply: bool,
        proto: Option<async_ucx::ucp::AmProto>,
    ) -> Result<(), async_ucx::Error> {
        let start = if is_stats_enabled() { Some(Instant::now()) } else { None };
        self.worker.activate();

        let ret = self
            .endpoint
            .am_send_vectorized(id, header, iov, need_reply, proto)
            .await;

        if let Some(start) = start {
            let data_bytes: usize = iov.iter().map(|s| s.len()).sum();
            let total_us = start.elapsed().as_micros() as u64;
            tracing::debug!(
                "AM_SEND_TIMING op=\"am_send_vectorized\" am_id={} header_bytes={} data_bytes={} iov_count={} need_reply={} total_us={}",
                id, header.len(), data_bytes, iov.len(), need_reply, total_us
            );
        }

        // Do not deactivate here - messages may still be in flight in UCX layer
        // Let the reactor manage worker state based on actual activity
        ret
    }
}

impl AmStream {
    #[instrument(level = "trace", skip(self))]
    #[async_backtrace::framed]
    pub async fn wait_msg(&self) -> Option<async_ucx::ucp::AmMsg> {
        let start = if is_stats_enabled() { Some(Instant::now()) } else { None };
        self.worker.wait_connect();

        let ret = self.stream.wait_msg().await;

        if let Some(start) = start {
            let total_us = start.elapsed().as_micros() as u64;
            let has_msg = ret.is_some();
            tracing::debug!(
                "AM_WAIT_MSG_TIMING stream_id={} has_msg={} total_us={}",
                self.id, has_msg, total_us
            );
        }

        self.worker.deactivate();
        ret
    }

    /// Close the stream and wake up all waiting tasks.
    /// After calling this, `wait_msg()` will return `None` for any waiting tasks.
    pub fn close(&self) {
        self.stream.close();
    }
}

impl Drop for AmStream {
    fn drop(&mut self) {
        {
            self.worker
                .listener_ids
                .borrow_mut()
                .remove(&self.id.to_string());
        }
        self.worker.deactivate();
    }
}
