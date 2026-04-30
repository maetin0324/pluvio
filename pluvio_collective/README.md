# pluvio_collective

非同期集団通信 (`Allreduce`, `Scatter`, `Allgather`, `Broadcast`) を Pluvio
の `Reactor` trait の上で動かす実装。Phase 1+2 で `Allreduce` / `Scatter` の
存在証明と正しさ検証までを行い、Phase 3 で **pipelined ring** /
**recursive doubling** / **Bruck allgather** / **binomial-tree broadcast** /
**adaptive policy** までを追加した。`docs/pluvio_collective_plan.md` と
`docs/phase3.md` を参照。

## バックエンド

| バックエンド | 概要 | feature flag |
|---|---|---|
| `mpi_backend` | `MPI_Iallreduce` / `MPI_Iscatter` / `MPI_Iallgather` / `MPI_Ibcast` を `mpi-sys` 直叩きで Future 化 | `mpi` |
| `ucx_backend` | `pluvio_ucx` の Active Message に乗せた Ring / Pipelined Ring / Recursive Doubling allreduce、直送り型 Scatter、Bruck Allgather、Binomial-tree Broadcast | `ucx` |

両方が `Communicator` / `Collective<T>` trait を impl するので、上位アプリは同じ
インタフェースで両方を試せる。

`Collective<T>` の現状の操作は以下のとおり:

```rust
fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a>;
fn scatter<'a>(
    &'a self,
    send_buf: Option<&'a [T]>,  // Some at root, None elsewhere
    recv_buf: &'a mut [T],
    root: usize,
) -> Self::ScatterFut<'a>;
fn allgather<'a>(
    &'a self,
    send_buf: &'a [T],
    recv_buf: &'a mut [T],      // length = send_buf.len() * size()
) -> Self::AllgatherFut<'a>;
fn broadcast<'a>(&'a self, buf: &'a mut [T], root: usize) -> Self::BroadcastFut<'a>;
```

UCX 側の allreduce はデフォルトで plain ring を使うが、`UcxCommunicator` の
明示メソッドから別アルゴリズムも呼べる:

```rust
// 大メッセージ向け: micro_chunk × max_in_flight で pipelining
ucx_comm.allreduce_pipelined::<f32, Sum>(&mut buf, PipelineConfig::default()).await?;

// 小メッセージ + n が 2 の冪のとき最速
ucx_comm.allreduce_recursive_doubling::<f32, Sum>(&mut buf).await?;
```

`AdaptiveCommunicator` でラップするとメッセージサイズに応じて自動で切り替わる:

```rust
let policy = AdaptivePolicy::default();   // small=2KB, large=64KB
let adaptive = AdaptiveCommunicator::new(ucx_comm, policy);
adaptive.allreduce::<Sum>(&mut buf).await?;   // RD / Ring / Pipelined を自動選択
```

## クイックスタート

### 単体テスト

```bash
cargo test -p pluvio_collective --lib
```

`Op::identity` の正しさと AmRouter の slot 振り分けが対象。`mpiexec` 不要。

### 結合テスト (mpiexec が必要)

```bash
scripts/run_integration_tests.sh
```

スクリプトの中で 2 プロセスで以下を順番に実行する:

1. `mpi_allreduce_2proc` — Phase 1 の MPI ラップ allreduce
2. `ucx_allreduce_2proc` — Phase 2 の UCX 直書き ring allreduce
3. `cross_check` — 同じ入力で MPI 版と UCX 版の allreduce を実行し、相対誤差 `1e-3` 以内で一致することを検証
4. `scatter_2proc` — MPI 版と UCX 版の scatter で、root の送信バッファが正しく分散されることを検証
5. `ucx_pipelined_allreduce_2proc` — micro_chunks=4 の f32 Sum、bit-exact u32 XOR、`micro_chunks=1` で plain ring と完全一致することの3点
6. `ucx_recursive_doubling_2proc` — RD の f32 Sum と、ring との bit-exact u32 XOR cross-check
7. `ucx_allgather_2proc` — Bruck allgather が rank 順に並んだ受信バッファを返すこと
8. `ucx_broadcast_2proc` — binomial-tree broadcast が root から全 rank に伝搬すること

`PROCS` (プロセス数)、`PORT` (rendezvous 用 TCP ポート)、`PROFILE`
(`debug`/`release`) を環境変数で上書きできる。

### example の実行

```bash
# Phase 1: MPI Iallreduce / Iscatter
mpiexec -n 2 cargo run --example coll_mpi_example

# Phase 2: UCX 直書き allreduce + scatter
mpiexec -n 2 \
  -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 -x PLUVIO_COLL_ROOT_PORT=14200 \
  cargo run --example coll_ucx_example

# Phase 3: pipelined ring (micro_chunks × max_in_flight)
mpiexec -n 2 \
  -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 -x PLUVIO_COLL_ROOT_PORT=14201 \
  cargo run --example coll_ucx_pipelined

# Phase 3: AdaptiveCommunicator で size 別アルゴリズム選択を確認
mpiexec -n 2 \
  -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 -x PLUVIO_COLL_ROOT_PORT=14202 \
  cargo run --example coll_adaptive
```

## API スケッチ

```rust
use pluvio_collective::{Collective, Sum};
use pluvio_collective::mpi_backend::{MpiCommunicator, MpiReactor};

let runtime = pluvio_runtime::executor::Runtime::new(1024);
let reactor = MpiReactor::new();
runtime.register_reactor("mpi", reactor.clone());
let comm = MpiCommunicator::world(reactor)?;

runtime.clone().run_with_name_and_runtime("demo", async move {
    let mut buf = vec![1.0_f32; 1024];
    comm.allreduce::<Sum>(&mut buf).await?;
});
```

UCX 側は `bootstrap_communicator` で `Vec<Option<Rc<Endpoint>>>` を作ってから
`UcxCommunicator::new` に渡す。`AmRouter` と `dispatcher_loop` をひとつ spawn する
ことで、AM `id=COLLECTIVE_AM_ID` のメッセージが `(src, step, phase)` ヘッダ毎に
振り分けられる。

## 現状の制約 (Phase 4 以降で対応予定)

- `Alltoall` / `Reduce_scatter` 単体は未実装(allreduce 内では使う)
- スケーリング測定は 2 ノード環境のみ。4 ノード以上での評価は別フェーズ
- GPU buffer 非対応 (CPU buffer のみ)
- 浮動小数点は `f32`/`f64`、整数は `u32`。bf16/fp16 は未対応
- 単一スレッド前提 (Pluvio runtime の制約)
- Adaptive policy の threshold はデフォルト値のまま (small=2 KiB, large=64 KiB)。
  ベンチを通した fitting は別リポジトリ(`pluvio_collective_bench`)で行う
- `f32`/`f64`/`u32` のみ。bf16 や任意型は未対応
- GPU buffer 非対応 (CPU buffer のみ)
- 単一スレッド前提 (Pluvio runtime の制約)
- `cargo bench` ベンチや CPU 使用率測定はこのフェーズの DOD 外

## 既知の制約

- **UCX バージョン整合性**: `pluvio_ucx` の vendored UCX (1.18) と、システムにある
  `libucp.so.0` (Ubuntu 24.04 だと 1.16) が衝突するとシャットダウン時に assertion
  failure を起こす。`scripts/run_integration_tests.sh` は vendored UCX を
  `LD_LIBRARY_PATH` 先頭に追加することで回避している。
- **OpenMPI の async progress thread**: `OMPI_MCA_pml_ucx_progress_iterations=0`
  を設定しないと、MPI 内部の UCX progress と Pluvio の reactor 駆動が二重に走る。
  `mpi_backend::disable_async_progress()` を `MPI_Init_thread` の前に呼ぶこと。
- **AmRouter は単一 communicator 想定**: 現状は AM ID を1個 (`COLLECTIVE_AM_ID`)
  しか使わない。複数 communicator を同時に走らせるには ID 衝突回避の手当てが必要。

## ファイル構成

```
pluvio_collective/
├── src/
│   ├── lib.rs                     # Collective trait
│   ├── communicator.rs            # Communicator trait
│   ├── op.rs                      # Op<T>: Sum/Max/Min/Prod/BitXor
│   ├── error.rs                   # CollectiveError
│   ├── mpi_backend/
│   │   ├── reactor.rs             # MpiReactor (MPI_Test ループ)
│   │   ├── communicator.rs        # MpiCommunicator
│   │   ├── datatype.rs            # MpiDatatype trait
│   │   ├── allreduce.rs           # AllreduceFuture (MPI_Iallreduce)
│   │   ├── scatter.rs             # ScatterFuture (MPI_Iscatter)
│   │   ├── allgather.rs           # AllgatherFuture (MPI_Iallgather)
│   │   └── broadcast.rs           # BroadcastFuture (MPI_Ibcast)
│   └── ucx_backend/
│       ├── am_router.rs           # AM ID + (src, step, phase, micro_chunk) → slot
│       ├── bootstrap.rs           # TCP rendezvous で WorkerAddress 交換
│       ├── communicator.rs        # UcxCommunicator (各アルゴリズムの明示メソッド)
│       ├── ring.rs                # plain ring allreduce
│       ├── pipelined_ring.rs      # micro_chunk × max_in_flight 並列の ring
│       ├── recursive_doubling.rs  # log2(n) round の RD allreduce (n=2^k)
│       ├── bruck.rs               # Bruck allgather
│       ├── broadcast.rs           # binomial-tree broadcast
│       ├── scatter.rs             # 直送り scatter (root → 各 rank の AM 1 通)
│       ├── adaptive.rs            # AdaptiveCommunicator + AdaptivePolicy
│       └── util.rs                # split_disjoint_mut ヘルパ
├── examples/
│   ├── coll_mpi_example.rs        # MPI: allreduce + scatter
│   ├── coll_ucx_example.rs        # UCX: allreduce + scatter
│   ├── coll_ucx_pipelined.rs      # UCX: pipelined ring
│   └── coll_adaptive.rs           # AdaptiveCommunicator のサイズ別切替デモ
└── tests/
    ├── mpi_allreduce_2proc.rs               # #[ignore]
    ├── ucx_allreduce_2proc.rs               # #[ignore]
    ├── cross_check.rs                       # #[ignore]
    ├── scatter_2proc.rs                     # #[ignore]
    ├── ucx_pipelined_allreduce_2proc.rs     # #[ignore]
    ├── ucx_recursive_doubling_2proc.rs      # #[ignore]
    ├── ucx_allgather_2proc.rs               # #[ignore]
    └── ucx_broadcast_2proc.rs               # #[ignore]
```
