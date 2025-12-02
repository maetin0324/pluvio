use pluvio_runtime::executor::Runtime;
use pluvio_ucx::{UCXReactor, WorkerAddressInner};
use std::net::UdpSocket;
use std::rc::Rc;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static SHARED_FILE: &str = "endpoint_info.data";

const N: usize = 1 << 15;
const WINDOW: usize = 2048; // まずは 512〜4096 の範囲で要実験
static DATA_SIZE: usize = 1 << 20; // 1MiB
static HEADER: [u8; 256] = [0u8; 256];

fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "Info".into()),
        )
        .with(tracing_subscriber::fmt::Layer::default().with_ansi(true))
        .init();

    let runtime = Runtime::new(1024);
    runtime.set_affinity(0);
    let ucx_reactor = UCXReactor::current();
    runtime.register_reactor("ucx_reactor", ucx_reactor.clone());
    if let Some(server_addr) = std::env::args().nth(1) {
        runtime.clone().run_with_name_and_runtime(
            "ucx_example_client",
            client(server_addr, runtime, ucx_reactor),
        );
    } else {
        runtime
            .clone()
            .run_with_name_and_runtime("ucx_example_server", server(runtime, ucx_reactor));
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

    tracing::debug!("client: read server address, connect to {:?}", worker_addr);

    let endpoint = worker.connect_addr(&worker_addr).unwrap();

    // tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let stream = Rc::new(worker.am_stream(16).unwrap());
    let endpoint = Rc::new(endpoint);

    tracing::debug!("client: send am message");
    let now = std::time::Instant::now();

    let data = vec![0u8; DATA_SIZE];

    let mut inflight = 0usize;
    for _ in 0..N {
        while inflight >= WINDOW {
            let _ = stream.wait_msg().await.unwrap(); // 1つ回収
            inflight -= 1;
        }
        endpoint
            .am_send(
                12,
                &HEADER,
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
        (DATA_SIZE * (1 << 20)) as f64 / now.elapsed().as_secs_f64() / 1e9
    );
}

async fn server(runtime: Rc<Runtime>, reactor: Rc<UCXReactor>) -> anyhow::Result<()> {
    tracing::debug!("server");
    let context = pluvio_ucx::Context::new().unwrap();
    let worker = context.create_worker().unwrap();
    reactor.register_worker(worker.clone());

    tracing::debug!("server: created worker");

    // let mut listener = worker
    //     .create_listener("0.0.0.0:10000".parse().unwrap())
    //     .unwrap();

    let worker_addr = worker.address().unwrap();
    std::fs::write(SHARED_FILE, worker_addr.as_ref()).unwrap();
    worker.wait_connect();

    let bind = UdpSocket::bind("0.0.0.0:10000").unwrap();
    tracing::debug!("listening on {}", bind.local_addr().unwrap());

    let stream = worker.am_stream(12).unwrap();
    let stream = Rc::new(stream);

    // for i in 0u8.. {
    // let worker = worker.clone();
    // let conn = listener.next().await;
    // conn.remote_addr().unwrap();
    // let ep = worker.accept(conn).await.unwrap();
    // let ep = Rc::new(ep);
    // let epc = ep.clone();
    let stream = stream.clone();
    let runtime = runtime.clone();
    let jh = runtime.clone().spawn_with_runtime(async move {
        // epをmoveしないとspawn後にdropされてcloseされてしまう
        // let _ep = epc;
        // let runtime_clone = runtime.clone();
        // 事前 spawn しない。受信ループを数本だけ並行稼働させるなら worker プールで。
        let mut recv_buffer = vec![0u8; DATA_SIZE];
        for i in 0..(1 << 20) {
            let mut msg = stream.wait_msg().await.unwrap();
            if i % 10000 == 0 {
                tracing::debug!("server: received {} messages", i);
            }
            msg.recv_data_single(&mut recv_buffer).await.unwrap();
            unsafe {
                msg.reply(16, &[0], &[0], false, None).await.unwrap();
            }
            // let stream = stream.clone();
            // runtime_clone.spawn(async move {
            //     let msg = stream.wait_msg().await.unwrap();
            //     unsafe {
            //         msg.reply(16, &[0], &[0], false, None).await.unwrap();
            //     }
            // });
        }

        tracing::debug!("all done");
    });
    jh.await.unwrap();
    // }
    // unreachable!()
    Ok(())
}
