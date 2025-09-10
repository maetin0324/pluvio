//! Example binary demonstrating the pluvio runtime.
#![allow(unused_imports)]

use futures::stream::StreamExt;
use pluvio_runtime::executor::Runtime;
use pluvio_uring::file::DmaFile;
use pluvio_uring::prepare_buffer;
use pluvio_uring::reactor::{register_file, IoUringReactor};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;
use std::{fs::File, os::fd::AsRawFd, sync::Arc};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static TOTAL_SIZE: usize = 128 << 30;
static BUFFER_SIZE: usize = 1 << 20; // 1 MiB

/// Entry point of the example application.
fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "Info".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    let runtime = Runtime::new(1024);
    let reactor = IoUringReactor::builder()
        .queue_size(2048)
        .buffer_size(BUFFER_SIZE)
        .submit_depth(64)
        .wait_submit_timeout(Duration::from_millis(100))
        .wait_complete_timeout(Duration::from_millis(150))
        .build();

    runtime.register_reactor("io_uring_reactor", reactor);
    runtime.clone().run(async move {
        let file = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open("/local/rmaeda/pluvio_test.txt")
            .expect("Failed to open file");
        let fd = file.as_raw_fd();

        file.set_len(TOTAL_SIZE as u64)
            .expect("Failed to set file length");

        register_file(fd);

        let dma_file = std::rc::Rc::new(DmaFile::new(file));

        dma_file.fallocate(TOTAL_SIZE as u64).await.expect("fallocate failed");

        let handles = futures::stream::FuturesUnordered::new();
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
            let file = dma_file.clone();
            let buffer = file
                .acquire_buffer().await;
            // tracing::debug!("fill buffer with 0x61");
            let offset = (i * BUFFER_SIZE) as u64;
            let handle = runtime.clone().spawn_with_name(
                async move  {file.write_fixed(buffer, offset).await},
                format!("write_fixed_{}", i),
            );
            handles.push(handle);
        }

        tracing::debug!("all tasks added to queue");

        futures::future::join_all(handles).await;

        let longest_task_stats = runtime.get_longest_running_task().unwrap();
        tracing::debug!("longest task time: {:?}s", longest_task_stats.get_elapsed_real_time().unwrap().as_secs_f64());
        tracing::debug!("execute time of all task: {}s", Duration::from_nanos(runtime.get_total_time("")).as_secs_f64());
        tracing::debug!("reactor poll time: {}s", Duration::from_nanos(runtime.get_reactor_polling_time()).as_secs_f64());
        tracing::info!("write done: {:?}", now.elapsed());
        tracing::info!(
            "bandwidth: {:?}MiB/s",
            (TOTAL_SIZE / 1024 / 1024) as f64 / now.elapsed().as_secs_f64()
        );
        std::process::exit(0);
    });
}

