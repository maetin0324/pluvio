use crate::worker::Worker;
use std::{mem::MaybeUninit, rc::Rc};
use tracing::instrument;

pub struct Endpoint {
    pub endpoint: async_ucx::ucp::Endpoint,
    pub worker: Rc<Worker>,
}

impl Endpoint {
    #[instrument(level = "trace", skip(self, buf, rkey))]
    #[async_backtrace::framed]
    pub async fn put(
        &self,
        buf: &[u8],
        remote_addr: u64,
        rkey: &async_ucx::ucp::RKey,
    ) -> Result<(), async_ucx::Error> {
        self.worker.activate();
        let ret = self.endpoint.put(buf, remote_addr, rkey).await;
        self.worker.deactivate();
        ret
    }

    #[instrument(level = "trace", skip(self, buf, rkey))]
    #[async_backtrace::framed]
    pub async fn get(
        &self,
        buf: &mut [u8],
        remote_addr: u64,
        rkey: &async_ucx::ucp::RKey,
    ) -> Result<(), async_ucx::Error> {
        self.worker.activate();
        let ret = self.endpoint.get(buf, remote_addr, rkey).await;
        self.worker.deactivate();
        ret
    }

    #[instrument(level = "trace", skip(self, buf))]
    #[async_backtrace::framed]
    pub async fn stream_send(&self, buf: &[u8]) -> Result<usize, async_ucx::Error> {
        self.worker.activate();
        let ret = self.endpoint.stream_send(buf).await;
        self.worker.deactivate();
        ret
    }

    #[instrument(level = "trace", skip(self, buf))]
    #[async_backtrace::framed]
    pub async fn stream_recv(
        &self,
        buf: &mut [MaybeUninit<u8>],
    ) -> Result<usize, async_ucx::Error> {
        self.worker.activate();
        let ret = self.endpoint.stream_recv(buf).await;
        self.worker.deactivate();
        ret
    }

    #[instrument(level = "trace", skip(self, buf))]
    #[async_backtrace::framed]
    pub async fn tag_send(&self, tag: u64, buf: &[u8]) -> Result<usize, async_ucx::Error> {
        self.worker.activate();
        let ret = self.endpoint.tag_send(tag, buf).await;
        self.worker.deactivate();
        ret
    }

    #[instrument(level = "trace", skip(self, iov))]
    #[async_backtrace::framed]
    pub async fn tag_send_vectored(
        &self,
        tag: u64,
        iov: &[std::io::IoSlice<'_>],
    ) -> Result<usize, async_ucx::Error> {
        self.worker.activate();
        let ret = self.endpoint.tag_send_vectored(tag, iov).await;
        self.worker.deactivate();
        ret
    }

    pub fn is_closed(&self) -> bool {
        self.endpoint.is_closed()
    }

    pub fn get_status(&self) -> Result<(), async_ucx::Error> {
        self.endpoint.get_status()
    }

    pub fn print_to_stderr(&self) {
        self.endpoint.print_to_stderr();
    }

    #[instrument(level = "trace", skip(self))]
    #[async_backtrace::framed]
    pub async fn flush(&self) -> Result<(), async_ucx::Error> {
        self.endpoint.flush().await
    }

    #[instrument(level = "trace", skip(self))]
    #[async_backtrace::framed]
    pub async fn close(&self, force: bool) -> Result<(), async_ucx::Error> {
        self.endpoint.close(force).await
    }

    pub fn worker(&self) -> Rc<Worker> {
        self.worker.clone()
    }
}
