use std::{collections::HashMap, io::Error, sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex}};

use io_uring::IoUring;

use crate::SharedState;

// Reactor の定義
pub struct Reactor {
  pub ring: Arc<Mutex<IoUring>>,
  pub completions: Arc<Mutex<HashMap<u64, Arc<Mutex<SharedState>>>>>,
  pub user_data_counter: AtomicU64,
}

impl Reactor {
  pub fn new(queue_size: u32) -> Self {
      let ring = IoUring::new(queue_size).expect("Failed to create io_uring");
      Reactor {
          ring: Arc::new(Mutex::new(ring)),
          completions: Arc::new(Mutex::new(HashMap::new())),
          user_data_counter: AtomicU64::new(1), // 0 を避ける
      }
  }

  /// I/O 操作を登録し、対応する SharedState をマッピングに追加します。
  pub fn register_io(&self, shared_state: Arc<Mutex<SharedState>>) -> u64 {
      let user_data = self.user_data_counter.fetch_add(1, Ordering::Relaxed);
      let mut completions = self.completions.lock().unwrap();
      completions.insert(user_data, shared_state);
      user_data
  }

  /// I/O 操作を送信します。
  pub fn submit_io(&self, sqe: io_uring::squeue::Entry, user_data: u64) {
      let mut ring = self.ring.lock().unwrap();
      unsafe {
          let sqe = sqe.user_data(user_data);
          ring.submission().push(&sqe).expect("Submission queue is full");
      }
      ring.submit().expect("Failed to submit SQE");
  }

  /// 完了キューをポーリングし、完了した I/O 操作を処理します。
  pub fn poll_completions(&self) {
      let mut ring = self.ring.lock().unwrap();
      let cq = ring.completion();

      for cqe in cq {
          let user_data = cqe.user_data();

          // マッピングから SharedState を取得
          let shared_state_opt = {
              let mut completions = self.completions.lock().unwrap();
              completions.remove(&user_data)
          };

          if let Some(shared_state) = shared_state_opt {
              let mut shared = shared_state.lock().unwrap();
              if cqe.result() >= 0 {
                  shared.result = Some(Ok(cqe.result() as usize));
              } else {
                  // エラーハンドリング
                  shared.result = Some(Err(Error::from_raw_os_error(-cqe.result())));
              }

              // Waker を呼び出してタスクを再開
              if let Some(waker) = shared.waker.take() {
                  waker.wake();
              }
          } else {
              eprintln!("Received completion for unknown user_data: {}", user_data);
          }
      }
  }

  pub fn is_empty(&self) -> bool {
      let completions = self.completions.lock().unwrap();
      completions.is_empty()
  }
}