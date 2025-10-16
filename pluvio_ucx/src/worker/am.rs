use std::rc::Rc;

use crate::worker::{endpoint::Endpoint, Worker};

pub type AmProto = async_ucx::ucp::AmProto;
pub type AmMsg = async_ucx::ucp::AmMsg;
pub type AmDataType = async_ucx::ucp::AmDataType;
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
    pub async fn am_send(
        &self,
        id: u32,
        header: &[u8],
        data: &[u8],
        need_reply: bool,
        proto: Option<AmProto>,
    ) -> Result<(), async_ucx::Error> {
        self.worker.activate();
        let ret = self
            .endpoint
            .am_send(id, header, data, need_reply, proto)
            .await;
        self.worker.deactivate();
        ret
    }

    pub async fn am_send_vectorized(
        &self,
        id: u32,
        header: &[u8],
        iov: &[std::io::IoSlice<'_>],
        need_reply: bool,
        proto: Option<AmProto>,
    ) -> Result<(), async_ucx::Error> {
        self.worker.activate();
        let ret = self
            .endpoint
            .am_send_vectorized(id, header, iov, need_reply, proto)
            .await;
        self.worker.deactivate();
        ret
    }
}

impl AmStream {
    pub async fn wait_msg(&self) -> Option<AmMsg> {
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
