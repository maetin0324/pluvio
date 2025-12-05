use std::rc::Rc;

use tracing::instrument;

use crate::worker::Worker;

pub struct Listener {
    pub listener: async_ucx::ucp::Listener,
    pub worker: Rc<Worker>,
    pub id: String,
}

impl Listener {
    #[instrument(level = "trace", skip(self))]
    pub fn socket_addr(&self) -> Result<std::net::SocketAddr, async_ucx::Error> {
        self.listener.socket_addr()
    }

    #[instrument(level = "trace", skip(self))]
    #[async_backtrace::framed]
    pub async fn next(&mut self) -> async_ucx::ucp::ConnectionRequest {
        self.listener.next().await
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        {
            self.worker.listener_ids.borrow_mut().remove(&self.id);
        }
        self.worker.deactivate();
    }
}
