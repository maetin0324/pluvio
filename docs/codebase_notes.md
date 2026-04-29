# Codebase notes for `pluvio_collective` (Task 0)

調査対象: `pluvio_runtime`, `pluvio_ucx`, `examples/ucx_example`, `examples/mpi_example`,
ワークスペースの `Cargo.toml`。実装方針を決めるために必要な API 形状をまとめる。

## A. `Reactor` trait と `Runtime` 登録

- 定義: `pluvio_runtime/src/reactor/mod.rs:16-21`
  ```rust
  pub trait Reactor {
      fn poll(&self);
      fn status(&self) -> ReactorStatus;
  }
  ```
  `ReactorStatus = { Running, Stopped }`。`name()` も `should_park()` も無い。
- `Rc<R: Reactor>` には blanket impl が `mod.rs:23-31` にある。
- `Runtime::register_reactor` のシグネチャ
  (`pluvio_runtime/src/executor/mod.rs:287-300`):
  ```rust
  pub fn register_reactor<R>(&self, id: &'static str, reactor: R)
  where R: Reactor + 'static
  ```
  内部で `Rc::new(reactor) as Rc<dyn Reactor>` にラップされる。よって
  `Rc<MpiReactor>` を渡してもよいし、`MpiReactor` 値そのままでもよい。
- 実行は `runtime.run_with_name_and_runtime("name", future)` または
  `runtime.run_with_runtime(future)`、TLS 版は `pluvio_runtime::run`。
- Waker 登録は reactor が直接やらない。各 Future が `Rc<RefCell<HandleState<T>>>`
  を保持し、reactor の `poll()` が完了を検出したら waker を起こす慣用。
  既存の `UCXReactor::poll` (`pluvio_ucx/src/reactor.rs:175-190`) は
  ```rust
  for (_, worker) in workers.iter() { worker.inner().progress(); }
  ```
  だけで、AM 完了の検出は async-ucx 側の Request 機構が引き受けている。

## B. UCX Worker / Endpoint / AM API

- `Context::new()` → `Result<Context, async_ucx::Error>`。`create_worker()` で
  `Rc<Worker>` を返す (`pluvio_ucx/src/worker/mod.rs:38-65`)。
- `Worker` は常に `Rc<Worker>` で扱う。`new()` が `UCXReactor::current().register_worker`
  を内部で呼ぶ (`mod.rs:67-80`)。
- `Worker::address()` は `WorkerAddress<'_>`、`as_ref::<[u8]>()` で生バイト
  シリアライズ可。デシリアライズは `WorkerAddressInner::from(&[u8])`
  (`async-ucx/src/ucp/worker.rs:189-225`)。
- `Worker::connect_addr(self: &Rc<Self>, addr: &WorkerAddressInner) -> Result<Endpoint>`
  (`pluvio_ucx/src/worker/mod.rs:178-193`)。同期メソッド。

### AM 送信

`pluvio_ucx/src/worker/am.rs:37-67`:
```rust
impl Endpoint {
    pub async fn am_send(
        &self, id: u32, header: &[u8], data: &[u8],
        need_reply: bool, proto: Option<async_ucx::ucp::AmProto>,
    ) -> Result<(), async_ucx::Error>;
}
```
返り値は Future。完了まで `.await` する。`am_send` 内で `worker.activate()` を
呼んでいるので、active カウンタ管理は自動。

### AM 受信

`am_recv` ではなく **stream + pull** モデル
(`pluvio_ucx/src/worker/am.rs:18-34, 102-129`):
```rust
impl Worker {
    pub fn am_stream(self: &Rc<Self>, id: u16) -> Result<AmStream, async_ucx::Error>;
}
impl AmStream {
    pub async fn wait_msg(&self) -> Option<async_ucx::ucp::AmMsg>;
    pub fn close(&self);
}
```
`AmMsg` の API (`async-ucx/src/ucp/endpoint/am.rs:106-369`):
- `header() -> &[u8]`, `data_len() -> usize`, `get_data() -> Option<&[u8]>`
- `recv_data_single(&mut self, &mut [u8]) -> Result<usize>` — Eager は同期コピー、
  Rendezvous は内部で `ucp_am_recv_data_nbx` を発行して await。

ヘッダはコールバックで C 側から渡された `&[u8]` を `Vec<u8>` にコピーされた状態で
保持される (`endpoint/am.rs:78-103` の `RawMsg`)。受信バッファは call site が用意
する (`recv_data_single` の引数)。

**重要**: `am_stream(id)` は thread_local の async-ucx 内 `am_streams` map に
登録された C コールバックを介して `SegQueue<RawMsg>` にメッセージを push する。
我々が router を書くなら、その「dispatcher 並み」のタスクを spawn し、
`stream.wait_msg().await` を回しながら header の `(src_rank, step, phase)` を
見て slot に振り分ける。

### WorkerAddress

`Worker::address() -> Result<WorkerAddress<'_>>`。`as_ref()` で `&[u8]` 取得、
`WorkerAddressInner::from(&[u8])` で復元、`worker.connect_addr(&inner)` で endpoint。
`examples/ucx_example/src/main.rs:55-60, 136-138` の通り、ファイル経由でも可だが
本実装では TCP rendezvous を使う。

## C. 既存 ucx_example の bootstrap

`examples/ucx_example/src/main.rs`:
- サーバが `worker.address()` を `endpoint_info.data` に書き込む (line 137)
- クライアントがそれを読んで `WorkerAddressInner::from(&[u8])` してから
  `worker.connect_addr(&inner)` で endpoint 化 (lines 55-60)
- AM `id=12` でサーバが受信、`reply` で `id=16` の AM を返している。

ファイル経由で OK な実装なので、本 crate の TCP rendezvous も同じノリで簡素に作る。

## D. 既存 mpi_example

`examples/mpi_example/Cargo.toml`: 高レベル `mpi = "0.8.0"` (rsmpi) を使っている。
本指示書では `mpi-sys` 直叩きを要求するため、これは流用しない。MpiReactor の参考実装は
ない (mpi_example は MPI で初期化だけして UCX で通信している)。

## E. Workspace dependencies

`Cargo.toml`:
```toml
[workspace.dependencies]
tracing = "0.1.41"
tracing-subscriber = {version = "0.3.19", features = ["env-filter"]}
futures = "0.3.31"
```
edition = `"2024"` (root)、`resolver = "3"`。

各 crate:
- `pluvio_runtime`: `core_affinity 0.8.3`, `futures 0.3.31`, `futures-lite 2.6.0`, `slab 0.4.9`,
  `tracing` (workspace), `async-backtrace 0.2`
- `pluvio_ucx`: `async-ucx (path, feature am)`, `pluvio_runtime (path)`, `slab 0.4.10`,
  `tracing`, `async-backtrace 0.2`
- `pluvio_uring`: `io-uring 0.7.3`, `libc 0.2.169`, `aligned_box 0.3.0`, `tracing`, `pluvio_runtime (path)`,
  `async-backtrace 0.2`

## F. UCXReactor の active-op カウンタ

`pluvio_ucx/src/reactor.rs:80-134`:
```rust
active_or_waiting_count: AtomicUsize,
pub fn increment_active_count(&self) { self.active_or_waiting_count.fetch_add(1, Relaxed); }
pub fn decrement_active_count(&self) { self.active_or_waiting_count.fetch_sub(1, Relaxed); }

fn status(&self) -> ReactorStatus {
    if self.active_or_waiting_count.load(Relaxed) > 0 { Running } else { Stopped }
}
```
ホットパスで `borrow().iter()` を避けるため AtomicUsize を使う。
`Worker::activate/deactivate/wait_connect` (`worker/mod.rs:87-137`) でカウンタを
動かす。MpiReactor も同パターンを取る (in-flight request 数を atomic で持つ)。

## 実装方針へのインプット

A〜F を踏まえて以下の方針を確定:

1. `MpiReactor`: `pending: RefCell<Vec<PendingReq>>` と `active: Cell<usize>` を持ち、
   `poll()` で `MPI_Test` を回す。`Reactor::status` は `active.get() > 0` で判定。
   `Cell` で OK (Atomic は不要、単一スレッド)。
2. `register_reactor("mpi", reactor)` で登録。`Rc<MpiReactor>` も Reactor を impl する
   (blanket)。
3. UCX 側は `pluvio_ucx::Worker` / `Endpoint` をそのまま使う。
   - 各 rank で `am_stream(am_id)` を1本 open
   - dispatcher タスクを `runtime.spawn_with_runtime` で常駐させ、
     `wait_msg().await` で受信 → ヘッダ `(src_rank, step, phase)` を見て
     `AmRouter` の slot map に振り分ける
   - `recv_data_single` は dispatcher 内で heap `Vec<u8>` に受け、
     slot 側で `copy_nonoverlapping` → 自動的に lifetime 切り離し
4. WorkerAddress は TCP rendezvous で交換。rank0 がコーディネータ、`std::net::TcpStream`。
   OpenMPI で起動した時に `OMPI_COMM_WORLD_RANK/SIZE` 環境変数を読んで bootstrap。
   それ以外は `PLUVIO_COLL_RANK/SIZE` を使う。
5. MPI 実装は **OpenMPI 4.1.6** が手元にある。プランは MPICH 推奨だが、`MPI_Iallreduce`
   は OpenMPI 4.1 でも標準で動く。プランの環境変数 `MPICH_ASYNC_PROGRESS=0` は
   存在しなくても無害なので両方設定する。
6. `mpi-sys` 直叩き: crates.io には `mpi-sys = "0.2"` があり (rsmpi の bindings 部分)、
   `mpicc` を pkg-config 経由で見つけてビルドする。

## オープンクエスチョン (実装中に決める)

- `am_stream` の `id` は `u16`。`am_send` 側は `u32`。`am_id_base = 0x10` 程度の
  base を使い、複数 communicator は base offset で衝突回避。本フェーズは1個のみ。
- ring の `chunk` サイズが 0 のときの挙動 (要素数が rank 数で割り切れない場合)。
  本フェーズは「`buf.len() % size != 0` ならエラー」とする (プラン §8.4)。
- recv_from の cancellation safety: dispatcher が heap buffer に受けて copy する方式
  なので、recv_from が drop されても dangling pointer リスクなし (slot 側の
  target_ptr は dispatcher 内の同期パスでしか使わない)。

