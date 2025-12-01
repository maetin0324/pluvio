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
        self.worker.activate();
        let ret = self
            .endpoint
            .am_send(id, header, data, need_reply, proto)
            .await;
        // Do not deactivate here - messages may still be in flight in UCX layer
        // Let the reactor manage worker state based on actual activity
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
        self.worker.activate();
        let ret = self
            .endpoint
            .am_send_vectorized(id, header, iov, need_reply, proto)
            .await;
        // Do not deactivate here - messages may still be in flight in UCX layer
        // Let the reactor manage worker state based on actual activity
        ret
    }
}

impl AmStream {
    #[async_backtrace::framed]
    pub async fn wait_msg(&self) -> Option<async_ucx::ucp::AmMsg> {
        self.worker.wait_connect();
        let ret = self.stream.wait_msg().await;
        self.worker.deactivate();
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
