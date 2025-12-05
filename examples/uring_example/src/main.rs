//! Example binary demonstrating the pluvio runtime.
#![allow(unused_imports)]

use futures::stream::StreamExt;
use pluvio_runtime::executor::Runtime;
use pluvio_runtime::set_runtime;
use pluvio_uring::file::DmaFile;
use pluvio_uring::prepare_buffer;
use pluvio_uring::reactor::{IoUringReactor, register_file};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;
use std::{fs::File, os::fd::AsRawFd, sync::Arc};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static TOTAL_SIZE: usize = 16 << 30;
static BUFFER_SIZE: usize = 4 << 20; // 4 MiB to match fio
static IODEPTH: usize = 256; // Match fio's iodepth

// sigintの時にasync-backtraceをダンプして終了する
// signal-handlerをlibc経由で登録
pub fn setup_signal_handlers() {
    // Setup SIGINT (Ctrl+C), SIGTERM, and SIGUSR1 handlers
    #[cfg(unix)]
    {
        use libc::SIGINT;
        unsafe {
            libc::signal(SIGINT, taskdump_signal_handler as libc::sighandler_t);
        }
    }

    #[cfg(unix)]
    extern "C" fn taskdump_signal_handler(_: libc::c_int) {
        eprintln!("\nReceived SIGINT, dumping async task backtraces...");
        async_backtrace::taskdump_tree(true);
        std::process::exit(1);
    }
}


/// Entry point of the example application.
fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "debug".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    let runtime = Runtime::new(1 << 12);
    // Match fio configuration
    let reactor = IoUringReactor::builder()
        .queue_size(256) // Enough for iodepth + headroom
        .buffer_size(BUFFER_SIZE)
        .submit_depth(IODEPTH as u32) // Submit when we have iodepth entries
        .wait_submit_timeout(Duration::from_micros(0)) // Submit immediately
        .wait_complete_timeout(Duration::from_micros(0)) // Check completions immediately
        // .sq_poll(50) // Enable SQPOLL with no CPU affinity
        .build();

    runtime.register_reactor("io_uring_reactor", reactor);

    set_runtime(runtime.clone());

    setup_signal_handlers();

    tracing::debug!("Before run_with_name");

    pluvio_runtime::run_with_name("uring_example_main", async move {
            tracing::debug!("Inside async block");
            // Use fio's test file for read benchmark
            let file = File::options()
                .read(true)
                .write(true)
                .custom_flags(libc::O_DIRECT)
                .open("/local/rmaeda/fio_test.txt")
                .expect("Failed to open file");
            tracing::debug!("File opened");
            let fd = file.as_raw_fd();

            tracing::debug!("Before register_file");
            register_file(fd);
            tracing::debug!("After register_file");

            let dma_file = std::rc::Rc::new(DmaFile::new(file));

            let total_blocks = TOTAL_SIZE / BUFFER_SIZE;

            let now = std::time::Instant::now();
            // let mut completed = 0usize;
            let mut next_block = 0usize;
            let mut handles = futures::stream::FuturesUnordered::new();

            // Initial submission: fill up to IODEPTH
            while next_block < total_blocks && handles.len() < IODEPTH {
                let file = dma_file.clone();
                let buffer = file.acquire_buffer().await;
                let offset = (next_block * BUFFER_SIZE) as u64;
                let handle = pluvio_runtime::spawn_with_name(
                    async move {
                        file.write_fixed(buffer, offset)
                            .await
                            .map(|(bytes, _buf)| bytes)
                    },
                    format!("write_fixed_{}", next_block),
                );
                handles.push(handle);
                next_block += 1;
            }

            // Process completions and submit new requests to maintain IODEPTH
            while let Some(_result) = handles.next().await {
                // completed += 1;

                // Submit next block if available
                if next_block < total_blocks {
                    let file = dma_file.clone();
                    let buffer = file.acquire_buffer().await;
                    let offset = (next_block * BUFFER_SIZE) as u64;
                    let handle = pluvio_runtime::spawn_polling_with_name(
                        async move {
                            file.read_fixed(buffer, offset)
                                .await
                                .map(|(bytes, _buf)| bytes)
                        },
                        format!("read_fixed_{}", next_block),
                    );
                    handles.push(handle);
                    next_block += 1;
                }
            }

            let elapsed = now.elapsed();
            tracing::info!("read done: {:?}", elapsed);
            tracing::info!(
                "bandwidth: {:.2} MiB/s ({:.2} GiB/s)",
                (TOTAL_SIZE / 1024 / 1024) as f64 / elapsed.as_secs_f64(),
                (TOTAL_SIZE as f64 / (1024.0 * 1024.0 * 1024.0)) / elapsed.as_secs_f64()
            );
            std::process::exit(0);
        });
}

#[async_backtrace::framed]
async fn read_fixed(buffer: pluvio_uring::allocator::FixedBuffer, offset: u64, file: DmaFile) -> Result<(i32, pluvio_uring::allocator::FixedBuffer), std::io::Error> {
    file.read_fixed(buffer, offset).await
}

#[async_backtrace::framed]
async fn write_fixed(buffer: pluvio_uring::allocator::FixedBuffer, offset: u64, file: DmaFile) -> Result<(i32, pluvio_uring::allocator::FixedBuffer), std::io::Error> {
    file.write_fixed(buffer, offset).await
}
