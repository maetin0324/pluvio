use io_uring::types;
use crate::SharedState;
use crate::reactor::Reactor;
use std::io::Result;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::future::Future;

// ReadFileFuture の定義
struct ReadFileFuture {
  shared_state: Arc<Mutex<SharedState>>,
  fd: types::Fd,
  buffer: Vec<u8>,
  offset: u64,
  reactor: Arc<Reactor>,
}

impl ReadFileFuture {
  fn new(
      fd: types::Fd,
      buffer: Vec<u8>,
      offset: u64,
      reactor: Arc<Reactor>,
  ) -> Self {
      ReadFileFuture {
          shared_state: Arc::new(Mutex::new(SharedState::new())),
          fd,
          buffer,
          offset,
          reactor,
      }
  }
}

impl Future for ReadFileFuture {
  type Output = Result<usize>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
      let this = self.get_mut();
      let mut shared = this.shared_state.lock().unwrap();

      // 既に結果がある場合は Ready を返す
      if let Some(result) = shared.result.take() {
          return Poll::Ready(result);
      }

      // I/O 操作をまだ登録していない場合、登録する
      if shared.waker.is_none() {
          // Reactor に SharedState を登録し、user_data を取得
          let user_data = this.reactor.register_io(this.shared_state.clone());

          // SQE の取得
          let mut sqe = {
              let mut ring = this.reactor.ring.lock().unwrap();
              match ring.submission().get_sqe() {
                  Some(sqe) => sqe,
                  None => {
                      // SQE が利用できない場合は Pending を返す
                      // 次回ポーリング時に再試行
                      return Poll::Pending;
                  }
              }
          };

          // Read 操作を準備
          unsafe {
              sqe.prep_read(
                  this.fd,
                  this.buffer.as_ptr() as u64,
                  this.buffer.len() as u32,
                  this.offset as i64,
              );
              sqe.user_data(user_data);
          }

          // I/O 操作を送信
          this.reactor.submit_io(&mut sqe, user_data);
      }

      // Waker を保存してタスクを再開可能にする
      shared.waker = Some(cx.waker().clone());
      Poll::Pending
  }
}


// WriteFileFuture の定義
struct WriteFileFuture {
  shared_state: Arc<Mutex<SharedState>>,
  fd: types::Fd,
  buffer: Vec<u8>,
  offset: u64,
  reactor: Arc<Reactor>,
}

impl WriteFileFuture {
  fn new(
      fd: types::Fd,
      buffer: Vec<u8>,
      offset: u64,
      reactor: Arc<Reactor>,
  ) -> Self {
      WriteFileFuture {
          shared_state: Arc::new(Mutex::new(SharedState::new())),
          fd,
          buffer,
          offset,
          reactor,
      }
  }
}

impl Future for WriteFileFuture {
  type Output = Result<usize>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
      let this = self.get_mut();
      let mut shared = this.shared_state.lock().unwrap();

      // 既に結果がある場合は Ready を返す
      if let Some(result) = shared.result.take() {
          return Poll::Ready(result);
      }

      // I/O 操作をまだ登録していない場合、登録する
      if shared.waker.is_none() {
          // Reactor に SharedState を登録し、user_data を取得
          let user_data = this.reactor.register_io(this.shared_state.clone());

          // SQE の準備
          let mut sqe = {
              let mut ring = this.reactor.ring.lock().unwrap();
              match ring.submission().next() {
                  Some(sqe) => sqe,
                  None => {
                      // SQE が利用できない場合は Pending を返す
                      return Poll::Pending;
                  }
              }
          };

          // Write 操作を準備
          unsafe {
              sqe.prep_write(
                  this.fd,
                  this.buffer.as_ptr() as u64,
                  this.buffer.len() as u32,
                  this.offset as i64,
              );
              sqe.user_data(user_data);
          }

          // I/O 操作を送信
          this.reactor.submit_io(&mut sqe, user_data);
      }

      // Waker を保存してタスクを再開可能にする
      shared.waker = Some(cx.waker().clone());
      Poll::Pending
  }
}