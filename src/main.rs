use io_uring::types;
use std::{fs::File, os::fd::AsRawFd, sync::Arc};
use ucio::executor::Runtime;
use ucio::future::{ReadFileFuture, WriteFileFuture};

// main 関数
fn main() {
    let runtime = Arc::new(Runtime::new(256));
    let reactor = runtime.reactor.clone();
    runtime.run(async move {
        // ファイルを開く（読み書き可能なモード）
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open("example.txt")
            .expect("Failed to open file");
        let fd = file.as_raw_fd();

        // 読み込みバッファの準備
        let read_buffer1 = vec![0u8; 256];
        let read_buffer2 = vec![0u8; 512];
        let read_offset1 = 0;
        let read_offset2 = 256;

        // 書き込みバッファの準備
        let write_buffer1 = vec![0x41u8; 1024 * 1024 * 1024];
        let write_buffer2 = vec![0x42u8; 1024 * 1024 * 1024];
        let write_offset1 = 0;
        let write_offset2 = 1024 * 1024 * 1024;

        // ReadFileFuture の作成とランタイムへの登録
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

        // WriteFileFuture の作成とランタイムへの登録
        WriteFileFuture::new(
            types::Fd(fd),
            write_buffer1,
            write_offset1,
            reactor.clone(),
        ).await.unwrap();
        WriteFileFuture::new(
            types::Fd(fd),
            write_buffer2,
            write_offset2,
            reactor.clone(),
        ).await.unwrap();
    });
}
