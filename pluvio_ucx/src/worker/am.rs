use std::rc::Rc;

use crate::worker::{endpoint::Endpoint, Worker};

pub type AmStreamInner = async_ucx::ucp::AmStream;

pub struct AmStream {
    stream: AmStreamInner,
    worker: Rc<Worker>,
    id: u16,
}

impl Worker {
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
    #[async_backtrace::framed]
    pub async fn am_send(
        &self,
        id: u32,
        header: &[u8],
        data: &[u8],
        need_reply: bool,
        proto: Option<async_ucx::ucp::AmProto>,
    ) -> Result<(), async_ucx::Error> {
        tracing::trace!(
            "am_send: start, id={}, header_len={}, data_len={}, need_reply={}, proto={:?}",
            id, header.len(), data.len(), need_reply, proto
        );
        self.worker.activate();
        let ret = self
            .endpoint
            .am_send(id, header, data, need_reply, proto)
            .await;
        // Do not deactivate here - messages may still be in flight in UCX layer
        // Let the reactor manage worker state based on actual activity
        tracing::trace!("am_send: complete, result={:?}", ret.is_ok());
        ret
    }

    #[async_backtrace::framed]
    pub async fn am_send_vectorized(
        &self,
        id: u32,
        header: &[u8],
        iov: &[std::io::IoSlice<'_>],
        need_reply: bool,
        proto: Option<async_ucx::ucp::AmProto>,
    ) -> Result<(), async_ucx::Error> {
        let total_len: usize = iov.iter().map(|s| s.len()).sum();
        tracing::trace!(
            "am_send_vectorized: start, id={}, header_len={}, data_len={}, iov_count={}, need_reply={}, proto={:?}",
            id, header.len(), total_len, iov.len(), need_reply, proto
        );
        self.worker.activate();
        let ret = self
            .endpoint
            .am_send_vectorized(id, header, iov, need_reply, proto)
            .await;
        // Do not deactivate here - messages may still be in flight in UCX layer
        // Let the reactor manage worker state based on actual activity
        tracing::trace!("am_send_vectorized: complete, result={:?}", ret.is_ok());
        ret
    }
}

impl AmStream {
    #[async_backtrace::framed]
    pub async fn wait_msg(&self) -> Option<async_ucx::ucp::AmMsg> {
        tracing::trace!("AmStream::wait_msg: start, stream_id={}", self.id);
        self.worker.wait_connect();
        let ret = self.stream.wait_msg().await;
        self.worker.deactivate();
        tracing::trace!(
            "AmStream::wait_msg: complete, stream_id={}, received={}",
            self.id, ret.is_some()
        );
        ret
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
