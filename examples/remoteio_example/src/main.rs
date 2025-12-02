use futures::{StreamExt, future};
use pluvio_runtime::executor::Runtime;
use pluvio_ucx::async_ucx::ucp::AmMsg;
use pluvio_ucx::{UCXReactor, WorkerAddressInner};
use pluvio_uring::file::DmaFile;
use pluvio_uring::reactor::{IoUringReactor, register_file};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static SHARED_FILE: &str = "endpoint_info.data";
static TARGET_FILE: &str = "/local/rmaeda/remoteio_target.data";

static TOTAL_SIZE: usize = 1 << 37; // 128GiB
static DATA_SIZE: usize = 1 << 20; // 1MiB
const N: usize = TOTAL_SIZE / DATA_SIZE;
const WINDOW: usize = 2048; // まずは 512〜4096 の範囲で要実験

const REQUEST_ID: u16 = 12;
const RESPONSE_ID: u32 = 16;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "Info".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    let runtime = Runtime::new(1024);

    runtime.set_affinity(0);
    let uring_reactor = IoUringReactor::builder()
        .queue_size(2048)
        .buffer_size(DATA_SIZE)
        .submit_depth(64)
        .wait_submit_timeout(Duration::from_millis(100))
        .wait_complete_timeout(Duration::from_millis(300))
        .build();
    runtime.register_reactor("io_uring_reactor", uring_reactor.clone());

    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx_reactor", ucx_reactor.clone());
    if let Some(server_addr) = std::env::args().nth(1) {
        runtime
            .clone()
            .run_with_name_and_runtime("remoteio_client", client(server_addr, runtime, ucx_reactor));
    } else {
        runtime
            .clone()
            .run_with_name_and_runtime("remoteio_server", server(runtime, ucx_reactor));
    }
    Ok(())
}

async fn client(server_addr: String, _runtime: Rc<Runtime>, reactor: Rc<UCXReactor>) {
    tracing::debug!("client: connect to {:?}", server_addr);
    let context = pluvio_ucx::Context::new().unwrap();
    let worker = context.create_worker().unwrap();
    reactor.register_worker(worker.clone());

    tracing::debug!("client: created worker");

    // let endpoint = worker
    //     .connect_socket(server_addr.parse().unwrap())
    //     .await
    //     .unwrap();
    // endpoint.print_to_stderr();

    let server_addr = std::fs::read(SHARED_FILE).unwrap();
    let worker_addr = WorkerAddressInner::from(server_addr.as_slice());

    tracing::debug!("client: read server address from file");

    let endpoint = worker.connect_addr(&worker_addr).unwrap();

    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let stream = Rc::new(worker.am_stream(16).unwrap());
    let endpoint = Rc::new(endpoint);

    tracing::debug!("client: send am message");
    let now = std::time::Instant::now();

    let data = vec![0x6au8; DATA_SIZE];

    let mut inflight = 0usize;
    for i in 0..N {
        while inflight >= WINDOW {
            let _ = stream.wait_msg().await.unwrap(); // 1つ回収
            inflight -= 1;
        }

        let offset = i * DATA_SIZE;

        let header_data = offset.to_le_bytes();

        endpoint
            .am_send(
                12,
                &header_data,
                &data,
                true,
                Some(pluvio_ucx::async_ucx::ucp::AmProto::Rndv),
            )
            .await
            .unwrap();
        inflight += 1;
    }
    while inflight > 0 {
        let _ = stream.wait_msg().await.unwrap();
        inflight -= 1;
    }

    // let mut all_jh = Vec::with_capacity(N);

    // for _ in 0..N {
    //     let stream = stream.clone();
    //     let endpoint = endpoint.clone();
    //     all_jh.push(runtime.spawn(async move {
    //         endpoint
    //             .am_send(12, &HEADER, &DATA, true, None)
    //             .await
    //             .unwrap();
    //         let _ = stream.wait_msg().await.unwrap();
    //     }));
    // }
    // futures::future::join_all(all_jh).await;
    tracing::debug!("client: received am message");
    tracing::info!(
        "IOPS: {}KIOPS",
        (1 << 20) as f64 / now.elapsed().as_secs_f64() / 1024.0
    );
    tracing::info!(
        "Total send size: {}GiB",
        (DATA_SIZE * (1 << 20)) as f64 / (1 << 30) as f64
    );
    tracing::info!(
        "Bandwidth: {}GB/s",
        (TOTAL_SIZE as f64 / now.elapsed().as_secs_f64() / 1e9)
    );
}

async fn server(runtime: Rc<Runtime>, ucx_reactor: Rc<UCXReactor>) -> anyhow::Result<()> {
    tracing::debug!("server");
    let context = pluvio_ucx::Context::new().unwrap();
    let worker = context.create_worker().unwrap();
    ucx_reactor.register_worker(worker.clone());

    let file = File::options()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(TARGET_FILE)
        .expect("Failed to open file");
    let fd = file.as_raw_fd();

    file.set_len(TOTAL_SIZE as u64)
        .expect("Failed to set file length");

    register_file(fd);

    let dma_file = std::rc::Rc::new(DmaFile::new(file));

    dma_file
        .fallocate(TOTAL_SIZE as u64)
        .await
        .expect("fallocate failed");

    tracing::debug!("server: created worker");

    // let mut listener = worker
    //     .create_listener("0.0.0.0:10000".parse().unwrap())
    //     .unwrap();

    let worker_addr = worker.address().unwrap();
    std::fs::write(SHARED_FILE, worker_addr.as_ref()).unwrap();
    worker.wait_connect();

    let stream = worker.am_stream(REQUEST_ID).unwrap();
    let stream = Rc::new(stream);

    let stream = stream.clone();
    let runtime = runtime.clone();
    let mut jhs = futures::stream::FuturesUnordered::new();
    let mut inflight = 0usize;

    for i in 0..N {
        let msg = stream.wait_msg().await.unwrap();
        if i % 1024 == 0 {
            tracing::debug!("server: received {} messages", i);
        }

        let header = msg.header();
        let offset = usize::from_le_bytes(header[0..8].try_into().unwrap());

        let jh = runtime.spawn_with_name_and_runtime(
            write_task(dma_file.clone(), offset, msg),
            format!("write_task_{}", i),
        );
        jhs.push(jh);
        inflight += 1;
        if inflight >= WINDOW {
            let _ = jhs.next().await.unwrap().unwrap();
            inflight -= 1;
        }
    }

    futures::future::join_all(jhs).await;

    tracing::debug!("all done");
    runtime.log_running_task_stat();
    Ok(())
}

async fn write_task(file: Rc<DmaFile>, offset: usize, mut msg: AmMsg) -> anyhow::Result<()> {
    let mut buffer = file.acquire_buffer().await;
    let buf_ref = buffer.as_mut_slice();
    msg.recv_data_single(buf_ref).await.unwrap();

    file.write_fixed(buffer, offset as u64).await?;

    unsafe {
        msg.reply(RESPONSE_ID, &[], &[], false, None).await.unwrap();
    }
    Ok(())
}
