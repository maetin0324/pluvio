#![allow(unused_imports)]

use io_uring::types;
use std::{fs::File, os::fd::AsRawFd, sync::Arc};
use ucio::executor::Runtime;
use ucio::future::{ReadFileFuture, WriteFileFuture};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() {
    tracing_subscriber::registry()
    .with(
      tracing_subscriber::EnvFilter::try_from_default_env()
      .unwrap_or_else(|_| "Debug".into())
    )
    .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
    .init();

    let runtime = Runtime::new(256);
    let reactor = runtime.reactor.clone();
    runtime.clone().run(async move {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open("example.txt")
            .expect("Failed to open file");
        let fd = file.as_raw_fd();

        let read_buffer1 = vec![0u8; 256];
        let read_buffer2 = vec![0u8; 512];
        let read_offset1 = 0;
        let read_offset2 = 256;

        let write_buffer1 = vec![0x41u8; 5 * 1024 * 1024 * 1024];
        let write_buffer2 = vec![0x42u8; 5 * 1024 * 1024 * 1024];
        let write_offset1 = 0;
        let write_offset2 = 5 * 1024 * 1024 * 1024;

        ReadFileFuture::new(
            types::Fd(fd),
            read_buffer1,
            read_offset1,
            reactor.clone(),
        ).await.unwrap();
        ReadFileFuture::new(
            types::Fd(fd),
            read_buffer2,
            read_offset2,
            reactor.clone(),
        ).await.unwrap();

        tracing::debug!("ReadFileFuture completed");
        tracing::debug!("main runtime queue: {:?}", runtime.clone().task_receiver.len());
        tracing::debug!("main reactor queue: {:?}", reactor.clone().completions.lock().unwrap());

        let h1 = runtime.clone().spawn(WriteFileFuture::new(
            types::Fd(fd),
            write_buffer1,
            write_offset1,
            reactor.clone(),
        ));
        let h2 = runtime.clone().spawn(WriteFileFuture::new(
            types::Fd(fd),
            write_buffer2,
            write_offset2,
            reactor.clone(),
        ));
        p().await;
        let c = runtime.clone().spawn_polling(CountFuture::new(1000)).await.unwrap();
        tracing::debug!("CountFuture completed, count: {}", c);
        let (ret1, ret2) = futures::join!(h1, h2);
        tracing::debug!("WriteFileFuture completed, ret1 {} bytes, ret2 {} bytes", ret1.unwrap().unwrap(), ret2.unwrap().unwrap());
        // tracing::debug!("BackTrace: {:#?}", std::backtrace::Backtrace::force_capture());
    });
}

async fn p() {
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

fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
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