# Pluvio Collective Communication — Phase 1+2 実装指示書

## 0. この文書について

Pluvio (`https://github.com/maetin0324/pluvio`) の `Reactor` trait を MPI 非同期集団通信(主に `Allreduce`)に拡張する作業の指示書。**Phase 1 (MPI ラップ方式) と Phase 2 (UCX 直書き方式) の最小実装と正しさ検証まで** を対象とし、pipelining と本格評価は次フェーズに分離する。

実装方針:
- 単一の作業ブランチ `feat/collective` 上にまとめて実装し、最後に1つの PR を立てる
- MPI 依存は **`mpi-sys` 直叩き** を使う(progress 制御を完全にコントロールするため)
- 単一スレッド前提を守る(`Rc`/`RefCell`、`Cell` のみ。`Arc`/`Mutex` 禁止)
- 既存 crate (`pluvio_runtime`, `pluvio_ucx`, `pluvio_uring`) は **改変しない**。新規 crate `pluvio_collective` として分離する

この指示書は autonomy 高めで書いている。判断に迷うときは「§14 詰まったら」を見て、それでも判断不能なら作業を止めてユーザに質問する。

---

## 1. 目的とコンテキスト

Pluvio は `status()` / `poll()` の2メソッドからなる minimal Reactor trait を提供し、UCX (RDMA) と io_uring (storage) を単一スレッド上で busy-poll する。論文 (`main.pdf` を参照) では、Tokio/Glommio/Mochi-Margo が UCX progress を一般スケジューラに落とすことで ~600µs のプログレス間隔ベースラインに張り付く現象を観測し、Pluvio の reactor-owned polling がこれを 80µs に短縮することを示した。

この指示書のゴールは、Pluvio の汎用性を **MPI 非同期集団通信 (non-blocking collectives)** に拡張することの **存在証明 (existence proof)** を作ること。具体的には:

- **Phase 1 (MPI ラップ)**: `MPI_Iallreduce` などの MPI 非同期 collective を `MpiReactor` の上で Future 化し、Pluvio の executor で driveable にする。これは「Reactor trait が MPI までカバーできる」ことを示す
- **Phase 2 (UCX 直書き)**: Ring allreduce アルゴリズムを `pluvio_ucx` の Active Message の上に async/await で直接実装する。これは MPI を介さずに Pluvio 単体で collective が書けることを示す

両方を **同じ `Collective` trait の実装** として揃え、後段のアプリ(stencil halo exchange, SGD 風 micro-benchmark)を一度書くだけで両方比較できるようにする。

---

## 2. スコープ

### このフェーズで実装すること

| 項目 | 詳細 |
|---|---|
| `pluvio_collective` crate 雛形 | workspace 追加、`Cargo.toml`、`lib.rs` |
| 共通 trait | `Communicator`, `Op<T>`, `Collective<T>` |
| MPI バックエンド | `MpiReactor`, `mpi_backend::MpiCommunicator`, `Allreduce` のみ Future 化 |
| UCX バックエンド | `ucx_backend::UcxCommunicator`, `ring_allreduce` のみ |
| 正しさ検証 | `f32` `Sum` で MPI と UCX 両実装の結果が bit-exact (※ FP の都合で要相談、§9 参照) |
| 最小 example | `examples/coll_mpi_example.rs`, `examples/coll_ucx_example.rs` |

### このフェーズで実装しない

- `Broadcast`, `Allgather`, `Reduce_scatter`, `Alltoall` (Allreduce のみ)
- Pipelined ring (chunk をさらに分割して複数 in-flight する版)
- Recursive doubling / Bruck などの代替アルゴリズム
- 本格評価 (criterion ベンチ、CPU 使用率測定、複数ノードスケーリング)
- GPU buffer 対応 (CPU buffer のみ)
- 任意型のサポート (`f32` と `f64` のみで十分)
- Persistent collective (MPI 4.0)

これらは Phase 3 以降。

---

## 3. 環境前提

ターゲット環境:
- Linux (kernel 5.1+ for io_uring, ただし io_uring は Phase 1+2 では使わない)
- Rust 2024 edition (`rustc 1.90.0+`)
- UCX 1.18 (既存 `pluvio_ucx` が依存)
- **MPICH 4.x または OpenMPI 5.x**(どちらかで動けばよい。CI では MPICH を優先)
- `mpi-sys` クレート(`Cargo.toml` に追加する)
- Mellanox ConnectX-7 を想定するが、開発時は loopback / TCP transport で動けばよい

開発時の動作確認は **2 プロセスを同一ホスト上** で立ち上げる構成で行う(`mpiexec -n 2`)。複数ノードでの動作確認は本フェーズの完成条件に含めない。

`mpi-sys` のビルドには `MPI_HOME` 環境変数または `mpicc` が PATH にあることが必要。`build.rs` で自動検出する。

---

## 4. 事前調査タスク (Task 0)

実装に入る前に **必ず以下を読み、要点を `docs/codebase_notes.md` にまとめること**。これをサボるとあとで API 不整合で書き直しになる。

調査対象:

1. `pluvio_runtime/src/` 全体
   - `Reactor` trait の正確な定義(メソッドシグネチャ、戻り値型)
   - `Runtime::register_reactor` のシグネチャ(ownership は `Rc<dyn Reactor>` か `Box<dyn Reactor>` か)
   - `Waker` まわり: タスクが待ちに入るときの慣用パターン
2. `pluvio_ucx/src/` 全体
   - `Worker`, `Endpoint`, `Context` の API
   - `am_send` の正確なシグネチャ(返り値が Future か、引数の lifetime)
   - **AM 受信側の API**: 受信ハンドラ登録の方法、受信バッファの管理、`am_recv` 系メソッドが存在するか
   - `WorkerAddress` のシリアライズ方法 (rank 間で交換するため)
   - 既存の active-op カウンタ実装(論文 §IV-C 記載)を確認
3. `examples/ucx_example` の構成
   - クライアント・サーバが `WorkerAddress` をどう交換しているか(ファイル経由 / TCP 経由 / hardcode)
4. `Cargo.toml` (workspace root)
   - 既存 dependency のバージョン、 edition
   - workspace member の追加方法

調査の結論として、特に以下の質問の答えを `codebase_notes.md` に書く:

- AM 受信は callback 登録型か、`async fn` を await する pull 型か
- AM 受信バッファは callee が用意するのか、ライブラリが管理するのか(receive matching の責任所在)
- `Endpoint` を `Rc<Endpoint>` で持ち回ってよいか、それとも `&Endpoint` 縛りか
- `WorkerAddress` 交換のための bootstrap 機構が既にあるか(なければ TCP で書く)

**もしこの段階で UCX 直書き ring allreduce が API 上書けないと判明したら、Phase 2 を保留して Phase 1 のみ完成させ、ユーザに `pluvio_ucx` 側の API 拡張が必要か相談すること。**

---

## 5. ディレクトリ構造

```
pluvio/
├── pluvio_collective/                   # 新規追加
│   ├── Cargo.toml
│   ├── build.rs                         # MPI ライブラリ検出 (mpi-sys ガイドに従う)
│   └── src/
│       ├── lib.rs                       # crate root, 共通 trait re-export
│       ├── communicator.rs              # Communicator trait
│       ├── op.rs                        # Op<T>, Sum, Max, Min, Prod
│       ├── error.rs                     # CollectiveError
│       ├── mpi_backend/
│       │   ├── mod.rs
│       │   ├── reactor.rs               # MpiReactor
│       │   ├── communicator.rs          # MpiCommunicator (MPI_COMM_WORLD ラッパ)
│       │   ├── datatype.rs              # MpiDatatype trait (f32, f64)
│       │   └── allreduce.rs             # AllreduceFuture
│       └── ucx_backend/
│           ├── mod.rs
│           ├── communicator.rs          # UcxCommunicator
│           ├── bootstrap.rs             # WorkerAddress 交換 (TCP 経由)
│           ├── am_router.rs             # AM ID + (src_rank, step) → recv slot のマッチング
│           └── ring.rs                  # ring_allreduce
├── examples/
│   ├── coll_mpi_example.rs              # 新規
│   └── coll_ucx_example.rs              # 新規
└── docs/
    └── codebase_notes.md                # Task 0 の成果物
```

`Cargo.toml` (workspace root) の `members` に `pluvio_collective` を追加する。

---

## 6. 共通 trait の設計

### `Communicator` (communicator.rs)

```rust
pub trait Communicator {
    fn rank(&self) -> usize;
    fn size(&self) -> usize;
}
```

### `Op<T>` (op.rs)

```rust
pub trait Op<T>: Copy + 'static {
    fn apply(a: T, b: T) -> T;
    fn identity() -> T;
    /// MPI バックエンド用。UCX 直書き側は使わない。
    #[cfg(feature = "mpi")]
    fn mpi_op() -> mpi_sys::MPI_Op;
}

#[derive(Copy, Clone)]
pub struct Sum;
#[derive(Copy, Clone)]
pub struct Max;
#[derive(Copy, Clone)]
pub struct Min;
#[derive(Copy, Clone)]
pub struct Prod;
```

`f32` と `f64` で `Sum`, `Max`, `Min`, `Prod` を impl する。`mpi_op()` は `MPI_SUM`, `MPI_MAX` などを返すだけ。

### `Collective<T>` (lib.rs)

```rust
pub trait Collective<T>: Communicator {
    type AllreduceFut<'a>: Future<Output = Result<(), CollectiveError>>
    where Self: 'a, T: 'a;

    fn allreduce<'a, O: Op<T>>(
        &'a self,
        buf: &'a mut [T],
    ) -> Self::AllreduceFut<'a>;
}
```

GAT (Generic Associated Type) で書く。`async fn in trait` でもよいがバックエンドごとの戻り型を明示したい場面が出るのでまずは GAT で。

---

## 7. Phase 1: MPI ラップ方式

### 7.1 mpi-sys の依存追加

`pluvio_collective/Cargo.toml`:

```toml
[features]
default = ["mpi"]
mpi = ["dep:mpi-sys"]
ucx = []  # 直書き側は async-ucx 経由なので feature gate のみ

[dependencies]
mpi-sys = { version = "0.2", optional = true }
pluvio_runtime = { path = "../pluvio_runtime" }
pluvio_ucx = { path = "../pluvio_ucx" }
futures = "0.3"
thiserror = "1"
```

`mpi-sys` の正確な crate 名とバージョンは crates.io で最新を確認すること。代替に `rsmpi`/`mpi` クレート内部の `mpi-sys` を再公開しているものを使う手もあるが、direct FFI を選んだ動機(progress 制御)を保つため避ける。

`build.rs` は `mpi-sys` の README に従う(通常は何もしなくても `mpicc` を `pkg-config` 経由で見つける)。

### 7.2 MpiReactor

```rust
// mpi_backend/reactor.rs
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::Waker;
use mpi_sys::*;
use pluvio_runtime::reactor::{Reactor, ReactorStatus};

pub struct MpiReactor {
    pending: RefCell<Vec<PendingReq>>,
    active: Cell<usize>,
}

struct PendingReq {
    req: MPI_Request,
    waker: Waker,
    done: Rc<Cell<bool>>,
}

impl MpiReactor {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            pending: RefCell::new(Vec::new()),
            active: Cell::new(0),
        })
    }

    /// Future 側から呼ばれる。req と waker を登録し、完了フラグを返す。
    pub(crate) fn register(
        &self,
        req: MPI_Request,
        waker: Waker,
    ) -> Rc<Cell<bool>> {
        let done = Rc::new(Cell::new(false));
        self.pending.borrow_mut().push(PendingReq {
            req,
            waker,
            done: done.clone(),
        });
        self.active.set(self.active.get() + 1);
        done
    }
}

impl Reactor for MpiReactor {
    fn status(&self) -> ReactorStatus {
        if self.active.get() > 0 {
            ReactorStatus::Running
        } else {
            ReactorStatus::Stopped
        }
    }

    fn poll(&self) {
        // SAFETY: 単一スレッドからのみ呼ばれる(Pluvio executor 契約)
        let mut p = self.pending.borrow_mut();
        let mut i = 0;
        while i < p.len() {
            let mut flag: i32 = 0;
            let mut status: MPI_Status = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                MPI_Test(&mut p[i].req, &mut flag, &mut status)
            };
            debug_assert_eq!(rc, MPI_SUCCESS as i32);
            if flag != 0 {
                let entry = p.swap_remove(i);
                entry.done.set(true);
                entry.waker.wake();
                self.active.set(self.active.get() - 1);
            } else {
                i += 1;
            }
        }
    }
}
```

正確な `Reactor` trait のシグネチャは Task 0 で確認したものに合わせる。

### 7.3 MpiCommunicator + AllreduceFuture

```rust
// mpi_backend/communicator.rs
pub struct MpiCommunicator {
    rank: usize,
    size: usize,
    reactor: Rc<MpiReactor>,
}

impl MpiCommunicator {
    /// MPI_Init_thread(MPI_THREAD_SINGLE) を呼んだ後に作る。
    /// Drop で MPI_Finalize は呼ばない (caller 責任)。
    pub fn world(reactor: Rc<MpiReactor>) -> Result<Self, CollectiveError> {
        let mut rank = 0i32;
        let mut size = 0i32;
        unsafe {
            MPI_Comm_rank(MPI_COMM_WORLD, &mut rank);
            MPI_Comm_size(MPI_COMM_WORLD, &mut size);
        }
        Ok(Self {
            rank: rank as usize,
            size: size as usize,
            reactor,
        })
    }
}

impl Communicator for MpiCommunicator {
    fn rank(&self) -> usize { self.rank }
    fn size(&self) -> usize { self.size }
}
```

```rust
// mpi_backend/allreduce.rs
pub struct AllreduceFuture<'a, T, O> {
    buf: &'a mut [T],
    state: AllreduceState,
    reactor: Rc<MpiReactor>,
    _op: PhantomData<O>,
}

enum AllreduceState {
    NotStarted,
    InFlight { done: Rc<Cell<bool>> },
    Done,
}

impl<'a, T: MpiDatatype, O: Op<T>> Future for AllreduceFuture<'a, T, O> {
    type Output = Result<(), CollectiveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        match &this.state {
            AllreduceState::NotStarted => {
                let mut req: MPI_Request = unsafe { std::mem::zeroed() };
                let rc = unsafe {
                    MPI_Iallreduce(
                        MPI_IN_PLACE,
                        this.buf.as_mut_ptr() as *mut _,
                        this.buf.len() as i32,
                        T::dtype(),
                        O::mpi_op(),
                        MPI_COMM_WORLD,
                        &mut req,
                    )
                };
                if rc != MPI_SUCCESS as i32 {
                    return Poll::Ready(Err(CollectiveError::Mpi(rc)));
                }
                let done = this.reactor.register(req, cx.waker().clone());
                this.state = AllreduceState::InFlight { done };
                Poll::Pending
            }
            AllreduceState::InFlight { done } => {
                if done.get() {
                    this.state = AllreduceState::Done;
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }
            AllreduceState::Done => Poll::Ready(Ok(())),
        }
    }
}
```

`MpiDatatype` trait は `dtype() -> MPI_Datatype` を要求し、`f32` には `MPI_FLOAT`、`f64` には `MPI_DOUBLE` を返す。

### 7.4 progress 二重化の回避

MPI が UCX をトランスポートに使う場合、MPI 内部の async progress thread と Pluvio の reactor が独立に worker を進めようとして競合する可能性がある。これを回避する:

- **MPICH**: `MPICH_ASYNC_PROGRESS=0`(デフォルトだが明示)、`MPIR_CVAR_ASYNC_PROGRESS=0`
- **OpenMPI**: `OMPI_MCA_pml_ucx_progress_iterations=0` および `--mca opal_progress_yield_when_idle 0`
- `MPI_Init_thread(MPI_THREAD_SINGLE)` を使う (`MPI_THREAD_MULTIPLE` ではない)

example の冒頭にこれらを `std::env::set_var` で設定するコードを置く(MPI_Init より前に呼ぶ)。

---

## 8. Phase 2: UCX 直書き方式

### 8.1 Communicator の構築

`UcxCommunicator` は `Vec<Endpoint>` を rank-indexed で持つ。`endpoints[my_rank]` は使わない(self は loopback で扱わない、buf 内のローカル reduce で済ませる)。

```rust
// ucx_backend/communicator.rs
pub struct UcxCommunicator {
    rank: usize,
    size: usize,
    endpoints: Vec<Option<Rc<Endpoint>>>,  // self は None
    worker: Rc<Worker>,
    am_router: Rc<AmRouter>,
    /// この communicator が使う AM ID の base。複数 communicator を作るなら衝突回避が必要。
    am_id_base: u32,
}
```

### 8.2 Bootstrap

`bootstrap.rs` で TCP 経由の単純な rendezvous を実装する。rank 0 がコーディネータ:

1. すべての rank が `worker.address()` を取得
2. rank 0 が固定ポート (例: 12345) で listen、他 rank が connect
3. 全員が自分の address を rank 0 に送る
4. rank 0 が全員の address 配列を broadcast
5. 各 rank が自分以外の address に対して `worker.connect_addr()` で endpoint を作る

Bootstrap だけで論文1本書けるテーマではあるので、ここはとにかく動けばよい。`tokio::net::TcpStream` ではなく `std::net::TcpStream` を blocking で使う(MPI_Init_thread 相当の初期化フェーズで、まだ Pluvio runtime は走っていない)。

引数として「rank 0 のホスト名 / ポート」「自分の rank」「world size」を取る。これらは環境変数 `PLUVIO_COLL_RANK`, `PLUVIO_COLL_SIZE`, `PLUVIO_COLL_ROOT_HOST`, `PLUVIO_COLL_ROOT_PORT` で渡す。example の起動スクリプトで設定する。

### 8.3 AM Router

UCX の Active Message は ID で dispatch される。Ring allreduce の各 step で送られるメッセージを受信側が `(src_rank, step)` で識別できる必要がある。

設計:
- `AmRouter` が AM ID `am_id_base` の handler を worker に登録する
- ヘッダに `(src_rank: u16, step: u16, phase: u8)` を埋める(8 byte で十分)
- `AmRouter` 内部に `HashMap<(rank, step, phase), RecvSlot>` を持つ
- `RecvSlot` は `Option<Waker>` と書き込み先 `*mut [u8]` と完了フラグ

擬似コード:

```rust
pub struct AmRouter {
    slots: RefCell<HashMap<RecvKey, RecvSlot>>,
    early_arrivals: RefCell<HashMap<RecvKey, Vec<u8>>>,  // post 前に来たデータ
}

#[derive(Hash, Eq, PartialEq, Copy, Clone)]
struct RecvKey { src: u16, step: u16, phase: u8 }

struct RecvSlot {
    target: *mut u8,
    len: usize,
    waker: Option<Waker>,
    done: Rc<Cell<bool>>,
}
```

`am_recv(src, step, phase, target_buf)` を `async fn` として提供し、内部で `RecvSlot` を slots に登録、`Pending` を返す。AM handler 内では key を見て slot を引き、データを target にコピーして waker を起動する。

**重要**: handler 内で UCX worker の他の API を呼ばないこと(callback context で UCX を再入させない)。データコピーは `std::ptr::copy_nonoverlapping` で、コピー終了後に slot を完了させる。Early arrival は別 map に貯めて、後から `am_recv` が呼ばれたときに即完了させる。

### 8.4 Ring allreduce

```rust
// ucx_backend/ring.rs
pub async fn ring_allreduce<T, O>(
    comm: &UcxCommunicator,
    buf: &mut [T],
) -> Result<(), CollectiveError>
where
    T: Copy + Default + bytemuck::Pod + 'static,
    O: Op<T>,
{
    let n = comm.size();
    let rank = comm.rank();
    if n == 1 { return Ok(()); }
    if buf.len() % n != 0 {
        return Err(CollectiveError::BadShape);
    }
    let chunk = buf.len() / n;
    let next = (rank + 1) % n;
    let prev = (rank + n - 1) % n;

    let mut tmp = vec![T::default(); chunk];

    // Phase 1: Reduce-scatter (n-1 step)
    for step in 0..n - 1 {
        let send_idx = (rank + n - step) % n;
        let recv_idx = (rank + n - step - 1) % n;

        let send_slice: &[T] = &buf[send_idx * chunk..(send_idx + 1) * chunk];
        let recv_slice: &mut [T] = &mut tmp[..];

        let send_fut = comm.send_to(next, step as u16, 0, bytemuck::cast_slice(send_slice));
        let recv_fut = comm.recv_from(prev, step as u16, 0, bytemuck::cast_slice_mut(recv_slice));

        let (sr, rr) = futures::future::join(send_fut, recv_fut).await;
        sr?; rr?;

        // Reduce: buf[recv_idx] = O(buf[recv_idx], tmp)
        let target = &mut buf[recv_idx * chunk..(recv_idx + 1) * chunk];
        for i in 0..chunk {
            target[i] = O::apply(target[i], tmp[i]);
        }
    }

    // Phase 2: Allgather (n-1 step、ring を逆方向に使う)
    for step in 0..n - 1 {
        let send_idx = (rank + n - step + 1) % n;
        let recv_idx = (rank + n - step) % n;

        // borrow checker 回避のため raw pointer を経由する
        let send_ptr = buf[send_idx * chunk..(send_idx + 1) * chunk].as_ptr();
        let recv_ptr = buf[recv_idx * chunk..(recv_idx + 1) * chunk].as_mut_ptr();
        let byte_len = chunk * std::mem::size_of::<T>();

        // SAFETY: send と recv は異なるチャンクを指す。Pluvio は単一スレッド。
        let send_slice = unsafe { std::slice::from_raw_parts(send_ptr as *const u8, byte_len) };
        let recv_slice = unsafe { std::slice::from_raw_parts_mut(recv_ptr as *mut u8, byte_len) };

        let send_fut = comm.send_to(next, step as u16, 1, send_slice);
        let recv_fut = comm.recv_from(prev, step as u16, 1, recv_slice);
        let (sr, rr) = futures::future::join(send_fut, recv_fut).await;
        sr?; rr?;
    }
    Ok(())
}
```

ポイント:

- `bytemuck::Pod` で `&[T]` ↔ `&[u8]` を安全に往復する
- Phase 1 で `tmp` を reduce のための receive buffer に、Phase 2 では直接 `buf` の対応チャンクに受信する
- Phase 2 で send と recv が `buf` の異なるチャンクを指すので Rust の借用チェックを通すために raw pointer 経由にする(コメントで SAFETY を明示)
- `futures::future::join` を使い、send と recv を同時 in-flight にする(これが Pluvio の真価)

### 8.5 制約と注意

- `T: Pod` 縛りなので `f32`, `f64` のみ。`Sum` は浮動小数点なので結合則を満たさず、ring の reduce 順序が MPI のそれと違うと bit-exact 一致しない(§9 参照)
- AM の rendezvous protocol が走るためメッセージサイズが大きいと前段のハンドシェイクで latency が増える。Phase 2 段階では eager / rendezvous の閾値はデフォルトで OK
- `comm.send_to` / `recv_from` のシグネチャは AM router の設計に合わせる。byte slice ベースで揃えるのが楽

---

## 9. 正しさ検証

### 9.1 単体テスト (cargo test)

`pluvio_collective/tests/` に置く:

- `op_identity.rs`: `Sum::apply(Sum::identity(), x) == x`, `Max`, `Min`, `Prod` 同様
- `mpi_reactor_smoke.rs`: `MPI_Init` → 1 プロセスで `MPI_Iallreduce` を1回投げて結果確認(`mpiexec -n 1` で動く)。`#[ignore]` を付けて手動実行
- `ucx_router_unit.rs`: AmRouter の slot 登録・early arrival のロジックを mock 入力でテスト

### 9.2 結合テスト (mpiexec が必要)

`pluvio_collective/tests/integration/` 以下、`#[ignore]` 付きで:

- `mpi_allreduce_2proc.rs`: `mpiexec -n 2` で起動、`f32` の `Sum` allreduce、各 rank が `[rank+1.0; 1024]` を持って allreduce、結果が `[3.0; 1024]` (= 1.0+2.0) になることを確認
- `ucx_allreduce_2proc.rs`: 同じ条件で UCX 直書き ring allreduce
- `cross_check.rs`: 同じランダム input に対して MPI 版と UCX 版を実行、`f32` なら `(a-b).abs() < 1e-3 * a.abs().max(1e-6)` の relative tolerance で一致確認(bit-exact ではなく近似一致)

**bit-exact 一致を求めない理由**: Ring allreduce は `((a+b)+c)+d` の順、MPI の recursive doubling は `(a+b)+(c+d)` の順で reduce することがあり、浮動小数点では結果がビット単位で異なる。これは正しい挙動なので relative tolerance で見る。

`u32` の XOR で reduce する追加テストを書けば bit-exact 比較ができるが、これは optional。`Op<u32>` for `BitXor` を impl してテストするのが望ましい。

### 9.3 起動スクリプト

`scripts/run_integration_tests.sh`:

```bash
#!/bin/bash
set -euo pipefail

export MPICH_ASYNC_PROGRESS=0
export MPIR_CVAR_ASYNC_PROGRESS=0

cargo build --workspace --release --tests

# MPI 版
mpiexec -n 2 ./target/release/deps/mpi_allreduce_2proc-* --ignored --nocapture

# UCX 版 (mpiexec で rank/size を環境変数で渡すラッパー)
mpiexec -n 2 bash -c '
  export PLUVIO_COLL_RANK=$OMPI_COMM_WORLD_RANK
  export PLUVIO_COLL_SIZE=$OMPI_COMM_WORLD_SIZE
  export PLUVIO_COLL_ROOT_HOST=localhost
  export PLUVIO_COLL_ROOT_PORT=12345
  exec ./target/release/deps/ucx_allreduce_2proc-* --ignored --nocapture
'
```

(MPICH の環境変数名は `PMI_RANK` / `PMI_SIZE`。OpenMPI と MPICH 両対応にするには両方 fallback で読む)

---

## 10. examples

### 10.1 `examples/coll_mpi_example.rs`

最小の動作デモ。1024 要素 `f32` の `Sum` allreduce を1回実行して結果を print。

```rust
fn main() {
    unsafe { mpi_sys::MPI_Init(...) };

    let runtime = pluvio_runtime::Runtime::new(1024);
    let mpi_reactor = MpiReactor::new();
    runtime.register_reactor("mpi", mpi_reactor.clone());

    let comm = MpiCommunicator::world(mpi_reactor).unwrap();
    let mut buf = vec![comm.rank() as f32 + 1.0; 1024];

    runtime.clone().run(async move {
        comm.allreduce::<Sum>(&mut buf).await.unwrap();
        println!("rank {} got buf[0] = {}", comm.rank(), buf[0]);
    });

    unsafe { mpi_sys::MPI_Finalize() };
}
```

### 10.2 `examples/coll_ucx_example.rs`

UCX 直書き版。bootstrap で endpoint を作ってから ring allreduce を1回実行。

両方とも `mpiexec -n 2` で動かすことを想定する(UCX 版も rank/size の取得に MPI を使うのが楽。さもなくば PMIx あるいは hand-rolled launcher が必要で本フェーズの工数が膨れる)。

---

## 11. 完成定義 (Definition of Done)

以下すべてが満たされたら PR を立てる:

- [ ] `cargo build --workspace --all-features` がエラーなく通る
- [ ] `cargo clippy --workspace --all-features -- -D warnings` がエラーなく通る
- [ ] `cargo test --workspace` (ignored 以外) がすべて pass
- [ ] `scripts/run_integration_tests.sh` を 2 プロセスで実行し、MPI 版・UCX 版・cross-check のすべてが pass
- [ ] `docs/codebase_notes.md` に Task 0 の調査結果が記載されている
- [ ] `pluvio_collective/README.md` に簡単な使い方と Phase 1+2 の制約が書かれている
- [ ] `unsafe` ブロックすべてに SAFETY コメントが付いている
- [ ] PR description に「動作確認した環境(OS, UCX version, MPI 実装と version)」を書く

---

## 12. スコープ外 (Phase 3 以降)

参考までに次フェーズで扱う予定の項目を書いておく。今回は **触らない**:

- Pipelined ring (chunk をさらに micro-chunk に分割、`FuturesUnordered` で D 個常時 in-flight)
- Recursive doubling allreduce (small message 用)
- Bruck allgather
- Broadcast, Reduce_scatter, Alltoall
- criterion ベンチ + 論文 §V-G と同じ CPU 使用率測定
- 4 ノード以上でのスケーリング測定
- GPU buffer (CUDA awareness)
- MPI 4.0 persistent collective (`MPI_Allreduce_init`)
- `Op<bf16>` などの非標準型

---

## 13. 開発フロー

1. ブランチ `feat/collective` を切る
2. **Task 0 (§4) を最初に完了させ、`docs/codebase_notes.md` をコミット**。これを飛ばすと後で詰む
3. §5 のディレクトリ構造どおりに空ファイルを置き、`Cargo.toml` だけ通る状態にする(skeleton commit)
4. 共通 trait (§6) を実装してコンパイルが通ることを確認
5. Phase 1 (§7) を全部書き、§9.2 の `mpi_allreduce_2proc` テストが通るところまで
6. Phase 2 (§8) を全部書き、§9.2 の `ucx_allreduce_2proc` テストが通るところまで
7. cross-check テスト (§9.2) で両方の結果が relative tolerance で一致することを確認
8. examples (§10) を書く
9. README, codebase_notes を整理して PR

各 step で `cargo build` と `cargo clippy` を回して warning 0 を維持する。コミットメッセージは Conventional Commits 風 (`feat(collective): ...`, `test(collective): ...`)。

最終的に PR は1つだが、コミットは論理単位で分けて読みやすくする(skeleton / common trait / mpi backend / ucx backend / examples / docs)。

---

## 14. 既知のリスクと対処

### リスク 1: `pluvio_ucx` の AM API が pull 型 `am_recv` を提供していない

→ §8.3 の AM Router を `pluvio_ucx` の callback 登録 API の上に被せる形で書く。それも無理なら Phase 2 を保留してユーザに相談。

### リスク 2: mpi-sys のシンボル名が想定と違う

→ `mpi-sys` 0.2 系のドキュメントを確認。`MPI_Iallreduce` がそのままの名前で公開されている前提だが、もし `MPI_Iallreduce_` のようなマングルになっていたら適宜 `extern "C"` で hand-declare する。

### リスク 3: MPI と UCX の二重 progress

→ §7.4 の環境変数を必ず設定。それでも `valgrind` や `strace` で MPI 内部の追加 thread が見えたら、Pluvio runtime が起動する前 (または後) に `pthread_kill` で殺す手もあるが本フェーズでは深追いしない。

### リスク 4: Bootstrap の TCP コネクション張り遅れ

→ rank 0 の listen が間に合わずに他 rank の connect が失敗する。connect 側は 100ms sleep + retry を 50 回まで繰り返す。

### リスク 5: f32 の precision で cross-check が失敗

→ §9.2 の relative tolerance を `1e-3` に緩める。input を `[0.0, 1.0]` の uniform random ではなく整数値にすれば bit-exact になることもある。

### リスク 6: `MPI_THREAD_SINGLE` で `MPI_Iallreduce` が non-blocking として動かない

→ `MPI_Iallreduce` 自体は thread support level に関係なく non-blocking として動くが、実装によっては `MPI_THREAD_FUNNELED` 以上を要求するかもしれない。`MPI_FUNNELED` で初期化してみる。

---

## 15. 詰まったら

以下の状況では作業を止めてユーザに質問すること:

- Task 0 で `pluvio_ucx` の AM API が想定と大きく違い、AM Router が書けないと判明したとき
- Phase 1 のテストが通った後、Phase 2 が `pluvio_ucx` の制約で書けないと判明したとき
- mpi-sys のクレートが見つからない、メンテされていない、ビルドできないとき
- bit-exact ではなく relative tolerance でも cross-check が失敗するとき (アルゴリズム不整合の疑い)

ユーザへの質問は GitHub issue ではなく、PR description のチェックボックスに書く形で待機する。

---

## 付録 A: 参考にする論文の節

- §IV-B: Reactor trait の semantics (status → poll の順序、poll の境界条件)
- §IV-C: 既存 UCXReactor の active-op カウンタ実装(MpiReactor も同じパターン)
- §IV-D: Idle parking と reactor starvation backstop(本フェーズでは触らないが、long-running collective の挙動を考えるときに参照)
- §V-E: Progress starvation のテスト方法(Phase 3 で同じ noise sweep を collective に対しても回す)

## 付録 B: 参考にすべき外部資料

- io_uring documentation (kernel.dk/io_uring.pdf) — 本フェーズでは使わないが、パターン参考に
- UCX docs: AM API (`ucp_am_send_nbx`, `ucp_am_recv_data_nbx`)
- MPI 3.1 standard, §5.12 "Nonblocking Collective Operations"
- `mpi-sys` crate の README とサンプル