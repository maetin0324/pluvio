# pluvio_collective

非同期集団通信 (`Allreduce`) を Pluvio の `Reactor` trait の上で動かす実装。
Phase 1+2 の最小実装で、`docs/pluvio_collective_plan.md` に書かれた DOD のうち
存在証明 (existence proof) と正しさ検証までをカバーする。

## バックエンド

| バックエンド | 概要 | feature flag |
|---|---|---|
| `mpi_backend` | `MPI_Iallreduce` を `mpi-sys` 直叩きで Future 化 | `mpi` |
| `ucx_backend` | `pluvio_ucx` の Active Message に乗せた Ring Allreduce | `ucx` |

両方が `Communicator` / `Collective<T>` trait を impl するので、上位アプリは同じ
インタフェースで両方を試せる。

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
3. `cross_check` — 同じ入力で MPI 版と UCX 版を実行し、相対誤差 `1e-3` 以内で一致することを検証

`PROCS` (プロセス数)、`PORT` (rendezvous 用 TCP ポート)、`PROFILE`
(`debug`/`release`) を環境変数で上書きできる。

### example の実行

```bash
# Phase 1
mpiexec -n 2 cargo run --example coll_mpi_example

# Phase 2 (Phase 1+2 では mpiexec を rank/size プロビジョニングだけに使う)
mpiexec -n 2 \
  -x PLUVIO_COLL_ROOT_HOST=127.0.0.1 \
  -x PLUVIO_COLL_ROOT_PORT=14200 \
  cargo run --example coll_ucx_example
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

## Phase 1+2 の制約 (= 触らないもの)

- `Allreduce` だけ。`Broadcast`/`Allgather`/`Alltoall` は未実装
- Pipelining (chunk のさらなる分割) なし
- アルゴリズムは ring 1 種のみ。recursive doubling や Bruck は未実装
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
│   │   └── allreduce.rs           # AllreduceFuture
│   └── ucx_backend/
│       ├── am_router.rs           # AM ID + (src, step, phase) → slot
│       ├── bootstrap.rs           # TCP rendezvous で WorkerAddress 交換
│       ├── communicator.rs        # UcxCommunicator
│       └── ring.rs                # ring_allreduce
├── examples/
│   ├── coll_mpi_example.rs
│   └── coll_ucx_example.rs
└── tests/
    ├── mpi_allreduce_2proc.rs     # #[ignore]
    ├── ucx_allreduce_2proc.rs     # #[ignore]
    └── cross_check.rs              # #[ignore]
```
