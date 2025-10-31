use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    io::IoSliceMut,
    mem::MaybeUninit,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
};

use crate::{reactor::UCXReactor, worker::listener::Listener};

pub mod am;
pub mod endpoint;
pub mod listener;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Active,
    WaitConnect,
    Inactive,
}

pub struct Context {
    context: Arc<async_ucx::ucp::Context>,
}

pub struct Worker {
    id: Cell<usize>,
    worker: Rc<async_ucx::ucp::Worker>,
    state: Cell<WorkerState>,
    listener_ids: RefCell<HashSet<String>>,
    wait_connect_start: Cell<Option<std::time::Instant>>,
}

pub type WorkerAddressInner = async_ucx::ucp::WorkerAddressInner;

impl Context {
    pub fn new() -> Result<Self, async_ucx::Error> {
        let context = async_ucx::ucp::Context::new()?;
        Ok(Self { context })
    }

    pub fn new_with_config(config: &async_ucx::ucp::Config) -> Result<Self, async_ucx::Error> {
        let context = async_ucx::ucp::Context::new_with_config(config)?;
        Ok(Self { context })
    }

    pub fn create_worker(&self) -> Result<Rc<Worker>, async_ucx::Error> {
        let worker = self.context.create_worker()?;
        Ok(Worker::new(worker))
    }

    pub fn print_to_stderr(&self) {
        self.context.print_to_stderr();
    }
}

impl Worker {
    pub fn new(worker: Rc<async_ucx::ucp::Worker>) -> Rc<Self> {
        let worker = Rc::new(Self {
            id: Cell::new(0),
            worker,
            state: Cell::new(WorkerState::Inactive),
            listener_ids: RefCell::new(HashSet::new()),
            wait_connect_start: Cell::new(None),
        });
        let id = UCXReactor::current().register_worker(worker.clone());
        worker.id.set(id);
        worker
    }

    pub fn state(&self) -> WorkerState {
        self.state.get()
    }

    pub fn activate(&self) {
        self.state.set(WorkerState::Active);
        self.wait_connect_start.set(None);
    }

    pub fn deactivate(&self) {
        if self.listener_ids.borrow().is_empty() {
            self.state.set(WorkerState::Inactive);
            self.wait_connect_start.set(None);
        } else {
            self.state.set(WorkerState::WaitConnect);
            self.wait_connect_start.set(Some(std::time::Instant::now()));
        }
    }

    pub fn wait_connect(&self) {
        self.state.set(WorkerState::WaitConnect);
        self.wait_connect_start.set(Some(std::time::Instant::now()));
    }

    pub fn wait_start_time(&self) -> Option<std::time::Instant> {
        self.wait_connect_start.get()
    }

    pub fn inner(&self) -> &async_ucx::ucp::Worker {
        &self.worker
    }
}

impl Worker {
    pub fn address(&self) -> Result<async_ucx::ucp::WorkerAddress, async_ucx::Error> {
        self.worker.address()
    }

    pub fn create_listener(
        self: &Rc<Self>,
        addr: SocketAddr,
    ) -> Result<Listener, async_ucx::Error> {
        let ret = self.worker.create_listener(addr);
        match ret {
            Ok(listener) => {
                if let &WorkerState::Inactive = &self.state() {
                    self.wait_connect();
                }

                self.listener_ids.borrow_mut().insert(addr.to_string());

                Ok(Listener {
                    listener,
                    worker: self.clone(),
                    id: addr.to_string(),
                })
            }
            Err(e) => Err(e),
        }
    }

    pub fn connect_addr(
        self: &Rc<Self>,
        addr: &WorkerAddressInner,
    ) -> Result<endpoint::Endpoint, async_ucx::Error> {
        // Do not activate/deactivate here - endpoint creation doesn't require state change
        // The actual communication through the endpoint will manage worker state appropriately
        let ret = self.worker.connect_addr(addr);
        match ret {
            Ok(endpoint) => Ok(endpoint::Endpoint {
                endpoint,
                worker: self.clone(),
            }),
            Err(e) => Err(e),
        }
    }

    pub async fn connect_socket(
        self: &Rc<Self>,
        addr: SocketAddr,
    ) -> Result<endpoint::Endpoint, async_ucx::Error> {
        self.activate();
        let ret = self.worker.connect_socket(addr).await;
        self.deactivate();
        match ret {
            Ok(endpoint) => Ok(endpoint::Endpoint {
                endpoint,
                worker: self.clone(),
            }),
            Err(e) => Err(e),
        }
    }

    pub async fn accept(
        self: &Rc<Self>,
        connection: async_ucx::ucp::ConnectionRequest,
    ) -> Result<endpoint::Endpoint, async_ucx::Error> {
        self.activate();
        let ret = self.worker.accept(connection).await;
        self.deactivate();
        match ret {
            Ok(endpoint) => Ok(endpoint::Endpoint {
                endpoint,
                worker: self.clone(),
            }),
            Err(e) => Err(e),
        }
    }

    pub async fn tag_recv(
        &self,
        tag: u64,
        buffer: &mut [MaybeUninit<u8>],
    ) -> Result<usize, async_ucx::Error> {
        self.activate();
        let ret = self.worker.tag_recv(tag, buffer).await;
        self.deactivate();
        ret
    }

    pub async fn tag_recv_mask(
        &self,
        tag: u64,
        mask: u64,
        buffer: &mut [MaybeUninit<u8>],
    ) -> Result<(u64, usize), async_ucx::Error> {
        self.activate();
        let ret = self.worker.tag_recv_mask(tag, mask, buffer).await;
        self.deactivate();
        ret
    }

    pub async fn tag_recv_vectored(
        &self,
        tag: u64,
        iov: &mut [IoSliceMut<'_>],
    ) -> Result<usize, async_ucx::Error> {
        self.activate();
        let ret = self.worker.tag_recv_vectored(tag, iov).await;
        self.deactivate();
        ret
    }
}

// impl Drop for Worker {
//     fn drop(&mut self) {
//         UCXReactor::current().unregister_worker(self.id.get());
//     }
// }
