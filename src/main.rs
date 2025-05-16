#![allow(unused_imports)]

use io_uring::types;
use std::os::unix::fs::OpenOptionsExt;
use std::{fs::File, os::fd::AsRawFd, sync::Arc};
use pluvio::executor::Runtime;
use pluvio::io::{prepare_buffer, write_fixed, ReadFileFuture, WriteFileFuture};
use futures::stream::StreamExt;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static TOTAL_SIZE: usize = 12 * 1024 * 1024 * 1024;
static BUFFER_SIZE: usize = 32 * 1024;

fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "Info".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    let runtime = Runtime::new(1024, BUFFER_SIZE, 64, 100);
    let reactor = runtime.reactor.clone();
    runtime.clone().run(async move {
        let file = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open("/local/rmaeda/ucio_test.txt")
            .expect("Failed to open file");
        let fd = file.as_raw_fd();

        runtime.register_file(fd);

        let mut handles = futures::stream::FuturesUnordered::new();
        // for i in 0..(TOTAL_SIZE / BUFFER_SIZE) {
        //     let buffer = vec![0x61; BUFFER_SIZE];
        //     let reactor = reactor.clone();
        //     let handle = runtime.clone().spawn(WriteFileFuture::new(
        //         fd,
        //         buffer.clone(),
        //         (i * BUFFER_SIZE) as u64,
        //         reactor.clone(),
        //     ));
        //     handles.push(handle);
        // }

        let now = std::time::Instant::now();
        for i in 0..(TOTAL_SIZE / BUFFER_SIZE) {
            let buffer = prepare_buffer(runtime.clone().allocator.clone()).unwrap();
            // tracing::debug!("fill buffer with 0x61");
            let reactor = reactor.clone();
            let offset = (i * BUFFER_SIZE) as u64;
            let handle = runtime
                .clone()
                .spawn(write_fixed(fd, offset, buffer, reactor));
            handles.push(handle);
        }

        tracing::debug!("all tasks added to queue");
        
        while if let Some(_) = handles.next().await {
            // tracing::debug!("write done");
            true
        } else {
            false
        } {}
        tracing::info!("write done: {:?}", now.elapsed());
        tracing::info!("bandwidth: {:?}MiB/s", (TOTAL_SIZE / 1024 / 1024) as f64 / now.elapsed().as_secs_f64());
        std::process::exit(0);
    });
}

async fn _p() {
    println!("Hello, world!");
}

pub struct CountFuture {
    count: u32,
    complete_count: u32,
}

impl CountFuture {
    pub fn new(complete_count: u32) -> Self {
        CountFuture {
            count: 0,
            complete_count,
        }
    }
}

impl std::future::Future for CountFuture {
    type Output = u32;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        this.count += 1;
        if this.count < this.complete_count {
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        } else {
            println!("CountFuture is ready with value: {}", this.count);
            std::task::Poll::Ready(this.count)
        }
    }
}
