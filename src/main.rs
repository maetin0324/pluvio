// Cargo.toml
/*
[package]
name = "custom_runtime_with_reactor"
version = "0.1.0"
edition = "2021"

[dependencies]
io-uring = "0.7"
futures = "0.3"
crossbeam-channel = "0.5"
waker_fn = "1.1"
*/

use std::{fs::File, os::fd::AsRawFd, sync::Arc};

use ucio::executor::Runtime;





// main 関数
fn main() {
    // ファイルを開く（読み書き可能なモード）
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .open("example.txt")
        .expect("Failed to open file");
    let fd = file.as_raw_fd();

    // 読み込みバッファの準備
    let read_buffer1 = vec![0u8; 1024];
    let read_buffer2 = vec![0u8; 2048];
    let read_offset1 = 0;
    let read_offset2 = 1024;

    // 書き込みバッファの準備
    let write_buffer1 = vec![1u8; 512];
    let write_buffer2 = vec![2u8; 256];
    let write_offset1 = 0;
    let write_offset2 = 512;

    // ランタイムの作成
    let runtime = Arc::new(Runtime::new(256));

    // Reactor の参照を取得
    let reactor = runtime.reactor.clone();

    // ReadFileFuture の作成とランタイムへの登録
    runtime.spawn(ReadFileFuture::new(
        types::Fd(fd),
        read_buffer1,
        read_offset1,
        reactor.clone(),
    ));
    runtime.spawn(ReadFileFuture::new(
        types::Fd(fd),
        read_buffer2,
        read_offset2,
        reactor.clone(),
    ));

    // WriteFileFuture の作成とランタイムへの登録
    runtime.spawn(WriteFileFuture::new(
        types::Fd(fd),
        write_buffer1,
        write_offset1,
        reactor.clone(),
    ));
    runtime.spawn(WriteFileFuture::new(
        types::Fd(fd),
        write_buffer2,
        write_offset2,
        reactor.clone(),
    ));

    // ランタイムの実行
    runtime.run();
}
