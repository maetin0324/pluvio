pub mod reactor;
pub mod worker;

pub use crate::reactor::*;
pub use crate::worker::*;
pub mod async_ucx {
    pub use ::async_ucx::*;
}

/// Initialize Rndv tracking callbacks to connect async-ucx with UCXReactor.
/// This should be called once during application initialization.
pub fn init_rndv_tracking() {
    async_ucx::ucp::set_rndv_callbacks(async_ucx::ucp::RndvCallbacks {
        on_start: Box::new(|| {
            crate::reactor::UCXReactor::current().increment_rndv_count();
        }),
        on_complete: Box::new(|| {
            crate::reactor::UCXReactor::current().decrement_rndv_count();
        }),
    });
}
