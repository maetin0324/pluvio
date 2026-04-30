# Pluvio Collective — Phase 3 実装指示書 (論文化準備)

## 0. この文書について

Phase 1+2 (`pluvio_collective` crate, allreduce + scatter) が完了した状態から、論文化に向けて実装と評価基盤を整える指示書。**論文化準備が主目的** で、 pipelined ring と評価基盤を最優先、アルゴリズム多様化と DL マイクロベンチをそれに続ける構成。

リポジトリ分割方針:

- **Pluvio 本体** (`https://github.com/maetin0324/pluvio`): `pluvio_collective` crate の実装拡張のみ。pipelined ring、recursive doubling、Bruck などのアルゴリズム本体と、その単体テスト・example
- **ベンチマークリポジトリ** (新設、仮称 `pluvio_collective_bench`): 評価基盤、DL マイクロベンチ、stencil halo exchange、結果プロット、HPC クラスタ向け実行スクリプト。Pluvio 本体への依存は git submodule + path 依存

claude code には **2リポジトリそれぞれに独立した PR ブランチ** を作らせる。Phase 1+2 と同じく、コミットは論理単位で分けつつ最終的に PR は1本ずつ。

---

## 1. ゴールと論文ストーリー

論文化の主張(claude code は実装中、この主張を意識すること):

1. **Reactor trait は MPI 集団通信まで包めるだけの汎用性がある** — Phase 1 で示した
2. **UCX 直書きで pipelined collective を async/await で書ける** — Phase 3 の中心
3. **ユーザがアルゴリズム選択をコントロールできる** — Phase 3 のアルゴリズム多様化
4. **計算と通信を同一スレッドで重ねられる** — DL マイクロベンチで実証
5. **MPI async progress thread (1コア占有) を使わずに同等以上の性能** — 評価で実証

仮説(評価で検証する):

- 小メッセージ (≤ 64 KiB): plain ring ≈ MPI ≈ pipelined。差は出にくい
- 大メッセージ (≥ 1 MiB): pipelined ring が plain ring と MPI を上回る
- 計算オーバーラップ率: pipelined ring + Pluvio 構成が最も高い
- CPU 使用率: Pluvio 構成が `MPICH_ASYNC_PROGRESS=on` 比で大幅減

ターゲット会議: HPCAsia 2027 (1月締切想定) または IPDPS 2027 (10月締切)。

---

## 2. スコープ

### Phase 3 で実装すること

#### Pluvio 本体側 (`pluvio_collective` の拡張)

| 項目 | 詳細 | 優先度 |
|---|---|---|
| Pipelined ring allreduce | chunk を micro-chunk に分割、`FuturesUnordered` で常時 D 個 in-flight | 最優先 |
| Recursive doubling allreduce | small message 用 (`log n` step) | 中 |
| Bruck allgather + Allgather trait 拡張 | small/medium allgather | 中 |
| Broadcast (binomial tree) | 多くのアプリで使う | 低 |
| アルゴリズム選択ポリシー | メッセージサイズで切替、`AdaptiveCommunicator` ラッパ | 中 |
| `Allreduce` の異種バッファ型対応 | `T = f32 / f64 / u32 / u64`、bf16 は将来 | 低 |

#### ベンチマークリポジトリ側 (新設)

| 項目 | 詳細 | 優先度 |
|---|---|---|
| 評価フレームワーク | スイープ実行、結果 CSV 出力、Python/matplotlib プロット | 最優先 |
| Latency ベンチ | 小メッセージ ping-pong 風、p50/p99 | 最優先 |
| Throughput ベンチ | 大メッセージ、GiB/s | 最優先 |
| 計算オーバーラップ ベンチ | noise task + collective 同時実行、論文 §V-E 対応 | 最優先 |
| CPU 使用率測定 | `/proc/<pid>/stat` ベース、論文 §V-G 対応 | 最優先 |
| DL training マイクロベンチ | gradient allreduce + sleep でのモデル並列風 | 高 |
| Stencil halo exchange マイクロベンチ | 2D 5-point stencil + 隣接通信 | 中 |
| MPI 比較ベースライン | `MPI_Iallreduce` 直叩き、async progress on/off の両方 | 最優先 |
| HPC クラスタ実行スクリプト | PBS/Slurm 両対応の job script テンプレート | 中 |
| 2-node testbed 用ローカル実行 | 開発時の簡易検証用 | 高 |

### Phase 3 で実装しないこと

- マルチノードスケーリング測定本体 (4-8 ノードの実測)
- 論文 figure の最終版作成
- GPU collective (CUDA awareness)
- MPI 4.0 persistent collective
- bf16 / fp16 サポート
- `Alltoall`, `Reduce_scatter` の独立実装 (allreduce 内では使う)
- Tokio / Glommio / Mochi-Margo との比較 (Phase 1+2 の評価対象だが、今回は MPI のみ)

スケーリング測定は HPC クラスタの予約が取れた後の **Phase 4** で別建てとする。Phase 3 の段階では「2 ノード環境で動くベンチ」と「クラスタ用 job script のテンプレート」をそろえるのが目的。

---

## 3. 環境前提

### Pluvio 本体側

Phase 1+2 と同じ:

- Linux (kernel 5.1+)
- Rust 2024 edition (`rustc 1.90.0+`)
- UCX 1.18 (vendored)
- MPICH 4.x または OpenMPI 5.x
- 開発時は 2 プロセス同一ホスト (`mpiexec -n 2`)

### ベンチマークリポジトリ側

- 上記すべて
- Python 3.10+ (matplotlib, numpy, pandas)
- CSV を pandas で読んで matplotlib で出力
- HPC クラスタ向け: PBS Pro / NQSV / Slurm (どれか1つで動けばよく、テンプレートに記載)

---

## 4. 事前調査タスク (Task 0)

Phase 1+2 と同様、コードに手を入れる前に以下を確認して `docs/phase3_codebase_notes.md` にまとめる:

1. **既存 `pluvio_collective::ucx_backend::ring` の構造**:
   - `run_send_recv` の send/recv 同時実行が `futures::join` でどう動いているか
   - `RecvFuture` の cancellation 安全性 (Drop 時の `cancel_pending`)
   - `bytemuck::cast_slice` の使い方
2. **`pluvio_ucx` の `am_send`** がメッセージサイズに対してどう振る舞うか:
   - `AmProto::Eager` / `AmProto::Rendezvous` の使い分け
   - 送信完了 (Future が Ready になる) のタイミング: バッファコピー後か、ネット送信完了後か
3. **`AmRouter`** が複数の (src, step, phase) に対して並列に pending できるか:
   - 現状の `slots: HashMap<RecvKey, Slot>` で衝突しないか
   - Phase 1+2 の plain ring は step がループ内で進む直列実行なので問題が顕在化しない
   - Pipelined では同 src から異なる micro_step が同時に来る可能性がある→ phase byte を拡張するか、step の表現を調整する必要

特に **3** は pipelined ring の設計を左右する。 Task 0 で結論を出すこと。

---

## 5. Pluvio 本体側: Pipelined Ring Allreduce

### 5.1 設計概要

plain ring は `n - 1` step を直列に実行する。各 step では chunk 全体 (size `buf.len() / n`) を1つの AM メッセージで送る。

pipelined ring は各 step の chunk をさらに `M` 個の micro_chunk に分割し、全 `(n-1) * M` 個の micro-step を **依存関係グラフ** に従って並行実行する。具体的には:

```
chunk[i, m]: ring step i の m番目 micro-chunk
依存:
  reduce-scatter phase の chunk[i, m] は chunk[i-1, m] の reduce 完了後に送信可能
  allgather phase の chunk[i, m] は chunk[i-1, m] 受信完了後に送信可能
```

各 micro-step は独立した async task として spawn し、**同一 step の異なる micro_chunk** と **異なる step の同 micro_chunk (依存解決後)** を pipeline する。

### 5.2 実装方針

新規ファイル: `pluvio_collective/src/ucx_backend/pipelined_ring.rs`

```rust
pub struct PipelineConfig {
    /// Number of micro-chunks per ring step.
    pub micro_chunks: usize,
    /// Maximum in-flight micro-steps (across all steps and chunks).
    pub max_in_flight: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self { micro_chunks: 4, max_in_flight: 8 }
    }
}

pub fn pipelined_ring_allreduce_typed<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
    config: PipelineConfig,
) -> PipelinedRingAllreduceFuture<'a, T>
where T: Copy + Default + bytemuck::Pod + 'static, O: Op<T>;
```

内部実装:

```rust
async fn pipelined_ring_allreduce<'a, T, O>(
    comm: &'a UcxCommunicator,
    buf: &'a mut [T],
    config: PipelineConfig,
) -> Result<(), CollectiveError>
{
    let n = comm.size();
    let rank = comm.rank();
    if n <= 1 { return Ok(()); }
    let chunk_size = buf.len() / n;
    if !chunk_size.is_multiple_of(config.micro_chunks) {
        return Err(CollectiveError::BadShape);
    }
    let micro_size = chunk_size / config.micro_chunks;

    // Phase 1: reduce-scatter pipelined
    // micro_chunk m の reduce-scatter chain を spawn
    // step 0..n-1 を直列実行する task を1つ作る
    // (異なる m は完全に独立して走れる → micro_chunks 並列)

    use futures::stream::{FuturesUnordered, StreamExt};
    let mut tasks = FuturesUnordered::new();
    for m in 0..config.micro_chunks {
        tasks.push(reduce_scatter_chain::<T, O>(comm, buf_ptr, m, n, ...));
        if tasks.len() >= config.max_in_flight {
            tasks.next().await.transpose()?;
        }
    }
    while let Some(res) = tasks.next().await {
        res?;
    }

    // Phase 2: allgather pipelined (同様)
    // ...
    Ok(())
}
```

### 5.3 Borrow checker の壁

各 micro_chunk が `buf` の異なる領域を書き換えるが、Rust の borrow checker は同じ `&mut [T]` から複数の `&mut` slice を同時に取れない。Phase 1+2 の `ring.rs` で既に `unsafe { std::slice::from_raw_parts_mut(...) }` を使って disjoint access を実現しているので、同じパターンを適用する。

ヘルパとして「`buf` の互いに重ならない複数チャンクを `&mut [T]` の Vec に分割する」関数を `util` として用意するとよい:

```rust
/// SAFETY: 呼び出し側が `ranges` の各 (start, len) が pairwise disjoint で、
/// すべて `buf.len()` 内に収まることを保証する。
pub(crate) unsafe fn split_disjoint_mut<'a, T>(
    buf: &'a mut [T],
    ranges: &[(usize, usize)],
) -> Vec<&'a mut [T]> {
    let base = buf.as_mut_ptr();
    ranges.iter()
        .map(|&(start, len)| unsafe {
            std::slice::from_raw_parts_mut(base.add(start), len)
        })
        .collect()
}
```

### 5.4 AmRouter の拡張

Task 0 (§4) で確認した内容次第。pipelined ring では同 (src, step) で異なる micro_chunk が同時 in-flight になるため、`RecvKey` を `(src, step, phase, micro_chunk)` まで拡張する必要がある。

`AmHeader` を拡張:

```rust
pub struct AmHeader {
    pub src: u16,
    pub step: u16,
    pub phase: u8,
    pub micro_chunk: u8,  // 新規追加。最大 256 micro_chunks まで
}

impl AmHeader { pub const BYTES: usize = 8; }  // 6 → 8
```

既存の plain ring と scatter は `micro_chunk = 0` 固定で動かす。互換性のため `AmHeader::decode` は短いヘッダに対する fallback を用意するか、全バックエンドを 8-byte 版に統一する(後者を推奨)。

### 5.5 公開 API

`UcxCommunicator` に新メソッドを追加:

```rust
impl UcxCommunicator {
    pub async fn allreduce_pipelined<T, O>(
        &self,
        buf: &mut [T],
        config: PipelineConfig,
    ) -> Result<(), CollectiveError>
    where T: Copy + Default + bytemuck::Pod + 'static, O: Op<T>;
}
```

`Collective` trait のデフォルト動作は plain ring のままにし、pipelined は明示的に呼ぶ形にする。アルゴリズム選択ポリシー(§7)が入ったら、`Collective::allreduce` の内部で自動切替する。

### 5.6 テスト

Phase 1+2 の `tests/ucx_allreduce_2proc.rs` と同じ構造で `tests/ucx_pipelined_allreduce_2proc.rs` を追加:

- micro_chunks = 1 (= plain ring と同じ動作) で結果が plain と一致
- micro_chunks = 4 で f32 Sum が relative tolerance で一致
- micro_chunks = 4 で u32 BitXor が bit-exact 一致
- max_in_flight を変えて結果が変わらないこと

---

## 6. Pluvio 本体側: アルゴリズム多様化

### 6.1 Recursive Doubling Allreduce

新規ファイル: `pluvio_collective/src/ucx_backend/recursive_doubling.rs`

アルゴリズム: pairwise exchange を `log_2(n)` 回。各 round で距離 `2^k` の peer と双方向にバッファを交換し、ローカルで reduce。`n` が 2 の冪でない場合は補正手順が必要。

```rust
// n が 2 の冪のとき
for round in 0..log2(n) {
    let peer = rank XOR (1 << round);
    // peer に buf を送り、peer から buf を受ける (両方向同時)
    // 受信したものを buf にローカル reduce
}
```

実装上の注意:

- 2 の冪でない場合: Rabenseifner 法の前段(余剰ノードを 2 の冪に縮約)を入れるか、`n != 2^k` のときは plain ring にフォールバックする旨を docstring に書く
- send buffer と recv buffer は別物(Phase 1+2 の ring と違って overwrite しない)
- `f32 Sum` のような非結合演算では、ring と recursive doubling の結果は bit-exact では一致しない。relative tolerance テストを使う

公開 API:

```rust
impl UcxCommunicator {
    pub async fn allreduce_recursive_doubling<T, O>(
        &self, buf: &mut [T],
    ) -> Result<(), CollectiveError>;
}
```

### 6.2 Bruck Allgather

新規ファイル: `pluvio_collective/src/ucx_backend/bruck.rs`

`Allgather` を新たに `Collective` trait に追加するか、別 trait `Allgather<T>` として分離するかは設計判断。trait 拡張のほうが API が一貫するので、**`Collective` trait に `allgather` メソッドを追加** する方針:

```rust
pub trait Collective<T>: Communicator {
    type AllreduceFut<'a>: Future<...>;
    type ScatterFut<'a>: Future<...>;
    type AllgatherFut<'a>: Future<...>;  // 新規

    fn allgather<'a>(
        &'a self,
        send_buf: &'a [T],
        recv_buf: &'a mut [T],  // size = send_buf.len() * comm_size
    ) -> Self::AllgatherFut<'a>;
}
```

Bruck アルゴリズム本体: `log_2(n)` round の pairwise exchange、各 round で `2^k` 距離の peer から `2^k` 個のチャンクを受け取る。

MPI 版も `MPI_Iallgather` として実装:

```rust
// pluvio_collective/src/mpi_backend/allgather.rs
pub struct AllgatherFuture<'a, T> { ... }
```

### 6.3 Broadcast

新規ファイル: `pluvio_collective/src/ucx_backend/broadcast.rs`

binomial tree algorithm: `log_2(n)` step で root から葉まで2倍ずつ拡散。アルゴリズム自体は教科書的なので深追いしない。pipelining は将来のフェーズで。

### 6.4 アルゴリズム選択ポリシー

新規ファイル: `pluvio_collective/src/ucx_backend/adaptive.rs`

```rust
#[derive(Copy, Clone, Debug)]
pub enum AllreduceAlgo {
    PlainRing,
    PipelinedRing { config: PipelineConfig },
    RecursiveDoubling,
}

pub struct AdaptivePolicy {
    /// メッセージサイズ (bytes) → アルゴリズム選択
    pub small_threshold: usize,   // < これは RD
    pub large_threshold: usize,   // >= これは pipelined
}

impl AdaptivePolicy {
    pub fn choose_allreduce(&self, msg_bytes: usize, n: usize) -> AllreduceAlgo {
        if msg_bytes < self.small_threshold && n.is_power_of_two() {
            AllreduceAlgo::RecursiveDoubling
        } else if msg_bytes >= self.large_threshold {
            AllreduceAlgo::PipelinedRing { config: PipelineConfig::default() }
        } else {
            AllreduceAlgo::PlainRing
        }
    }
}

pub struct AdaptiveCommunicator {
    inner: UcxCommunicator,
    policy: AdaptivePolicy,
}

impl<T> Collective<T> for AdaptiveCommunicator
where T: Copy + Default + bytemuck::Pod + 'static
{
    fn allreduce<'a, O: Op<T>>(&'a self, buf: &'a mut [T]) -> Self::AllreduceFut<'a> {
        let bytes = buf.len() * std::mem::size_of::<T>();
        let algo = self.policy.choose_allreduce(bytes, self.inner.size());
        match algo {
            AllreduceAlgo::PlainRing => /* dispatch to ring */,
            AllreduceAlgo::PipelinedRing { config } => /* dispatch */,
            AllreduceAlgo::RecursiveDoubling => /* dispatch */,
        }
    }
}
```

`AllreduceFut` の型を `Pin<Box<dyn Future<...>>>` で erase することで、内部のアルゴリズム分岐を1つの戻り型に統一する。

threshold のデフォルト値は文献値 (MPICH の `MPIR_CVAR_ALLREDUCE_SHORT_MSG_SIZE` ≈ 2 KiB) を参考にしつつ、ベンチマーク結果から fitting する。Phase 3 では仮の値を入れておき、評価で fitting する。

---

## 7. ベンチマークリポジトリ: 設計

### 7.1 リポジトリ構造

```
pluvio_collective_bench/
├── Cargo.toml                  # workspace
├── README.md
├── crates/
│   ├── bench_runner/           # ベンチ実行本体 (Rust binary)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── config.rs       # YAML/CLI からスイープ設定読み込み
│   │       ├── workload.rs     # 各ワークロードの定義
│   │       ├── metrics.rs      # 測定: latency, throughput, overlap, CPU
│   │       └── output.rs       # CSV 出力
│   ├── workloads/              # 各ベンチワークロード
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── allreduce_latency.rs
│   │       ├── allreduce_throughput.rs
│   │       ├── overlap.rs       # 計算/通信オーバーラップ測定
│   │       ├── dl_microbench.rs # DL training 風
│   │       └── stencil.rs       # 2D 5-point stencil
│   └── mpi_baseline/            # MPI 直叩きベースライン
│       └── src/
│           ├── lib.rs
│           └── allreduce.rs
├── scripts/
│   ├── plot/
│   │   ├── plot_latency.py
│   │   ├── plot_throughput.py
│   │   ├── plot_overlap.py
│   │   └── plot_cpu.py
│   ├── local/
│   │   ├── run_all_2proc.sh    # 開発用: 2プロセス同一ホスト
│   │   └── run_quick.sh        # smoke test
│   └── cluster/
│       ├── pbs_template.sh     # PBS Pro / NQSV テンプレート
│       ├── slurm_template.sh
│       └── README_cluster.md
├── configs/
│   ├── sweep_default.yaml      # デフォルトスイープ設定
│   ├── sweep_quick.yaml        # 開発時の短時間版
│   └── sweep_paper.yaml        # 論文評価用フル版
├── results/                    # CSV 出力先 (gitignore 推奨)
│   └── .gitkeep
└── pluvio/                     # git submodule (Pluvio 本体)
```

Pluvio 本体への依存方法は **git submodule + path 依存** を推奨。`pluvio_collective_bench/Cargo.toml` で:

```toml
[workspace.dependencies]
pluvio_collective = { path = "../pluvio/pluvio_collective" }
pluvio_runtime = { path = "../pluvio/pluvio_runtime" }
pluvio_ucx = { path = "../pluvio/pluvio_ucx" }
```

### 7.2 ベンチランナーの設計

`bench_runner` は以下の責務を持つ:

1. **設定読み込み**: YAML or CLI からスイープ設定を読む
2. **MPI 初期化 + Pluvio runtime セットアップ** (1 度だけ)
3. **ワークロード実行**: 設定に従って各ワークロードを実行
4. **測定**: latency / throughput / overlap / CPU 使用率
5. **CSV 出力**: 1 trial = 1 row、複数 trial の median + min/max を 1 row に集約

スイープ設定例 (`configs/sweep_default.yaml`):

```yaml
trials: 5
warmup_iters: 100
measure_iters: 1000

workloads:
  - name: allreduce_latency
    backend:
      - mpi_iallreduce_async_off
      - mpi_iallreduce_async_on
      - pluvio_mpi_wrap
      - pluvio_ucx_plain_ring
      - pluvio_ucx_pipelined_ring
      - pluvio_ucx_recursive_doubling
    msg_sizes_bytes:
      - 64
      - 1024
      - 16384
      - 262144
      - 4194304
      - 67108864
    pipeline_micro_chunks: [1, 2, 4, 8]   # pluvio_ucx_pipelined_ring のみ
    pipeline_max_in_flight: [4, 8, 16]

  - name: allreduce_overlap
    msg_sizes_bytes: [262144, 4194304, 67108864]
    noise_tasks: [0, 16, 64, 256, 1024]
    noise_work_ns: 500
    backend: [pluvio_ucx_pipelined_ring, mpi_iallreduce_async_on, mpi_iallreduce_async_off]
```

### 7.3 ワークロード: allreduce_latency

小〜中メッセージのレイテンシを測る。

```rust
// crates/workloads/src/allreduce_latency.rs
pub async fn run<C: Collective<f32>>(
    comm: &C,
    msg_size_elements: usize,
    iters: usize,
) -> Vec<Duration> {
    let mut buf = vec![0.0f32; msg_size_elements];
    let mut latencies = Vec::with_capacity(iters);
    for _ in 0..iters {
        // 全 rank が同時に開始するよう barrier
        comm.barrier().await;
        let t0 = Instant::now();
        comm.allreduce::<Sum>(&mut buf).await.unwrap();
        latencies.push(t0.elapsed());
    }
    latencies
}
```

`Communicator` trait に `barrier` を追加するか、別途 `Barrier` 型を提供する必要がある。
最小実装としては `allreduce(&[0u8; 1])` を使えば代替できる(allreduce 自身が barrier の機能を持つ)。

出力 CSV のスキーマ:

```
backend,msg_size_bytes,iter,latency_ns
pluvio_ucx_pipelined_ring,4194304,0,123456
pluvio_ucx_pipelined_ring,4194304,1,124000
...
```

集約は plot 段階で pandas を使ってやる。

### 7.4 ワークロード: allreduce_throughput

大メッセージの帯域を測る。

```rust
pub async fn run<C: Collective<f32>>(
    comm: &C,
    msg_size_elements: usize,
    iters: usize,
) -> f64 /* GiB/s */ {
    let mut buf = vec![1.0f32; msg_size_elements];
    let bytes = msg_size_elements * 4;
    comm.barrier().await;
    let t0 = Instant::now();
    for _ in 0..iters {
        comm.allreduce::<Sum>(&mut buf).await.unwrap();
    }
    let elapsed = t0.elapsed();
    let total_bytes = (bytes * iters) as f64;
    total_bytes / elapsed.as_secs_f64() / (1024.0 * 1024.0 * 1024.0)
}
```

### 7.5 ワークロード: allreduce_overlap (最重要)

論文 §V-E に対応。CPU-bound noise task を並走させながら collective を走らせ、 noise 処理量と collective 完了時刻の両方を測る。

```rust
pub async fn run<C: Collective<f32>>(
    comm: &C,
    msg_size_elements: usize,
    n_noise_tasks: usize,
    noise_work_ns: u64,
    measure_duration: Duration,
) -> OverlapResult {
    let noise_count = Rc::new(Cell::new(0u64));
    let collective_count = Rc::new(Cell::new(0u64));
    let stop = Rc::new(Cell::new(false));

    // Spawn noise tasks
    for _ in 0..n_noise_tasks {
        let count = noise_count.clone();
        let stop = stop.clone();
        runtime.spawn(async move {
            while !stop.get() {
                // ~500 ns の volatile counter loop
                volatile_work(noise_work_ns);
                count.set(count.get() + 1);
                tokio::task::yield_now().await;
            }
        });
    }

    // Main: collective を繰り返し実行
    let mut buf = vec![1.0f32; msg_size_elements];
    let t0 = Instant::now();
    while t0.elapsed() < measure_duration {
        comm.allreduce::<Sum>(&mut buf).await.unwrap();
        collective_count.set(collective_count.get() + 1);
    }
    stop.set(true);

    OverlapResult {
        noise_iters: noise_count.get(),
        collective_iters: collective_count.get(),
        elapsed: t0.elapsed(),
    }
}
```

オーバーラップ率の計算:

```
overlap_ratio = (noise_iters during collective) / (noise_iters baseline without collective)
```

baseline は noise だけ走らせる別 trial で測る。値が 1.0 に近いほど計算と通信が完全に重なっている。

### 7.6 ワークロード: dl_microbench

「forward + backward」を `sleep` で模倣し、その後 gradient allreduce を走らせる。実 DL ライブラリは持ち込まない。

```rust
pub struct DlConfig {
    pub model_size_bytes: usize,    // = gradient size
    pub forward_backward_us: u64,
    pub n_steps: usize,
}

pub async fn run<C: Collective<f32>>(comm: &C, config: DlConfig) -> Vec<Duration> {
    let n_elems = config.model_size_bytes / 4;
    let mut grad = vec![1.0f32; n_elems];
    let mut step_times = Vec::with_capacity(config.n_steps);

    for _ in 0..config.n_steps {
        let t0 = Instant::now();
        // Forward + backward を模倣
        async_sleep(Duration::from_micros(config.forward_backward_us)).await;
        // Gradient allreduce
        comm.allreduce::<Sum>(&mut grad).await.unwrap();
        step_times.push(t0.elapsed());
    }
    step_times
}
```

`async_sleep` は busy-wait ではなく、実際の `tokio::time::sleep` 相当 (Pluvio runtime に sleep 機能があればそれ、なければ自前で)。

理想的には forward+backward と allreduce が並走できる構造を見せたいが、PyTorch 等の DDP も「backward が終わってから allreduce」が基本なので、ここでは順次実行で問題なし。Phase 3 の主張は「同一スレッドで forward/backward と allreduce を切り替えできる」点。

### 7.7 ワークロード: stencil

2D 5-point stencil の halo exchange:

```rust
pub async fn run<C: Collective<f32>>(comm: &C, n: usize, iters: usize) {
    // n × n local grid, 隣接 4 rank と halo (1 行) を交換
    // 真面目に書くなら 2D Cartesian topology が要るが、Phase 3 では
    // 1D ring topology (左右隣) で済ませる
}
```

stencil は collective ではなく point-to-point だが、「Pluvio 上で point-to-point も書ける」ことの存在証明として置く。優先度は中。

### 7.8 MPI ベースライン

`crates/mpi_baseline/` に **Pluvio を介さない** 純粋な MPI 実装を置く。比較対象を不公平にしないため重要:

```rust
// crates/mpi_baseline/src/allreduce.rs
pub fn run_latency(msg_size_elements: usize, iters: usize) -> Vec<Duration> {
    let mut buf = vec![0.0f32; msg_size_elements];
    let mut latencies = Vec::with_capacity(iters);
    for _ in 0..iters {
        unsafe { mpi_sys::MPI_Barrier(MPI_COMM_WORLD); }
        let t0 = Instant::now();
        let mut req = MPI_Request(std::ptr::null_mut());
        unsafe {
            MPI_Iallreduce(
                MPI_IN_PLACE,
                buf.as_mut_ptr() as *mut _,
                buf.len() as i32,
                RSMPI_FLOAT, RSMPI_SUM, RSMPI_COMM_WORLD,
                &mut req,
            );
            MPI_Wait(&mut req, std::ptr::null_mut());
        }
        latencies.push(t0.elapsed());
    }
    latencies
}
```

`mpi_iallreduce_async_off` と `mpi_iallreduce_async_on` の両方を測る。後者は `MPICH_ASYNC_PROGRESS=1` を環境変数で設定して別プロセスとして起動する。

### 7.9 CPU 使用率測定

論文 §V-G と同じ方式:

```rust
// crates/bench_runner/src/metrics.rs
pub struct CpuUsage {
    pub utime_ns: u64,
    pub stime_ns: u64,
    pub wall_ns: u64,
}

pub fn read_proc_stat() -> Result<(u64, u64), io::Error> {
    let s = fs::read_to_string("/proc/self/stat")?;
    // フィールド 14 = utime, 15 = stime (clock ticks)
    let fields: Vec<&str> = s.split_whitespace().collect();
    let utime: u64 = fields[13].parse().unwrap();
    let stime: u64 = fields[14].parse().unwrap();
    let tick_hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    let ns_per_tick = 1_000_000_000 / tick_hz;
    Ok((utime * ns_per_tick, stime * ns_per_tick))
}

pub fn measure<F: FnOnce()>(f: F) -> CpuUsage {
    let (u0, s0) = read_proc_stat().unwrap();
    let t0 = Instant::now();
    f();
    let (u1, s1) = read_proc_stat().unwrap();
    let wall = t0.elapsed().as_nanos() as u64;
    CpuUsage { utime_ns: u1 - u0, stime_ns: s1 - s0, wall_ns: wall }
}
```

ベンチ実行中の `(utime + stime) / wall` を出力。100% に近いほど busy polling、低いほど idle parking が効いている。

### 7.10 プロット

Python スクリプトで以下を出力:

- `plot_latency.py`: x = msg_size, y = latency (log-log), 各 backend を線で
- `plot_throughput.py`: x = msg_size, y = GiB/s, 同上
- `plot_overlap.py`: x = noise_tasks, y = overlap_ratio + 副軸 collective_throughput
- `plot_cpu.py`: 棒グラフで各 backend の CPU 使用率を比較

論文 figure と同じスタイル(matplotlib 単色線、error bar = min/max、median を線で、log scale axis)。

```python
# scripts/plot/plot_latency.py のスケッチ
import pandas as pd
import matplotlib.pyplot as plt

df = pd.read_csv(sys.argv[1])
fig, ax = plt.subplots()
for backend, group in df.groupby("backend"):
    agg = group.groupby("msg_size_bytes")["latency_ns"].agg(["median", "min", "max"])
    ax.errorbar(
        agg.index, agg["median"] / 1000,  # us
        yerr=[(agg["median"]-agg["min"])/1000, (agg["max"]-agg["median"])/1000],
        label=backend, marker="o",
    )
ax.set_xscale("log"); ax.set_yscale("log")
ax.set_xlabel("Message size (bytes)"); ax.set_ylabel("Latency (us)")
ax.legend()
plt.savefig(sys.argv[2])
```

---

## 8. HPC クラスタ向け実行スクリプト

### 8.1 PBS Pro / NQSV テンプレート

`scripts/cluster/pbs_template.sh`:

```bash
#!/bin/bash
#PBS -q regular
#PBS -l select=2:ncpus=64:mpiprocs=1
#PBS -l walltime=01:00:00
#PBS -j oe
#PBS -N pluvio_coll_bench

set -euo pipefail
cd "$PBS_O_WORKDIR"

module load ucx/1.18 openmpi/5.0  # 環境に合わせる

# Pluvio runtime / UCX 設定
export UCX_TLS=rc,sm,self
export UCX_NET_DEVICES=mlx5_0:1
export OMPI_MCA_pml_ucx_progress_iterations=0  # async progress 無効

# Build
cargo build --release --workspace

# Sweep 設定
CONFIG="${CONFIG:-configs/sweep_paper.yaml}"
RESULTS="results/$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS"

# 起動: 2 ノード x 1 プロセス
mpiexec -n 2 -hostfile "$PBS_NODEFILE" \
    -x PLUVIO_COLL_ROOT_HOST=$(head -1 "$PBS_NODEFILE") \
    -x PLUVIO_COLL_ROOT_PORT=14200 \
    target/release/bench_runner --config "$CONFIG" --output "$RESULTS"

# プロット
python scripts/plot/plot_latency.py "$RESULTS/allreduce_latency.csv" "$RESULTS/latency.pdf"
python scripts/plot/plot_throughput.py "$RESULTS/allreduce_throughput.csv" "$RESULTS/throughput.pdf"
python scripts/plot/plot_overlap.py "$RESULTS/allreduce_overlap.csv" "$RESULTS/overlap.pdf"
```

ユーザの testbed 環境では PBS Pro と NQSV の両方を見ているのでどちらに合わせるかはユーザ判断。テンプレートは PBS Pro 構文(`#PBS -l select=N:...`)で書き、NQSV (`#PBS -l filesystem=...` 等) との差分を README に書く。

### 8.2 Slurm テンプレート

`scripts/cluster/slurm_template.sh`:

```bash
#!/bin/bash
#SBATCH -p compute
#SBATCH -N 2
#SBATCH --ntasks-per-node=1
#SBATCH -t 01:00:00
#SBATCH -J pluvio_coll_bench

# (以下 PBS 版とほぼ同じ)
```

### 8.3 ローカル開発用スクリプト

`scripts/local/run_all_2proc.sh`:

```bash
#!/bin/bash
set -euo pipefail
export OMPI_MCA_pml_ucx_progress_iterations=0
export PLUVIO_COLL_ROOT_HOST=127.0.0.1
export PLUVIO_COLL_ROOT_PORT=14200
cargo build --release --workspace

CONFIG="${CONFIG:-configs/sweep_quick.yaml}"
RESULTS="results/local_$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS"

mpiexec -n 2 \
    -x PLUVIO_COLL_ROOT_HOST -x PLUVIO_COLL_ROOT_PORT \
    target/release/bench_runner --config "$CONFIG" --output "$RESULTS"

# Quick plot
python scripts/plot/plot_latency.py "$RESULTS/allreduce_latency.csv" "$RESULTS/latency.png"
```

`configs/sweep_quick.yaml` は `trials: 1`, `measure_iters: 50`, `msg_sizes_bytes: [1024, 1048576]` 程度。1 分以内に終わる量。

---

## 9. 完成定義 (Definition of Done)

### Pluvio 本体側

- [ ] `cargo build --workspace --all-features` がエラーなく通る
- [ ] `cargo clippy --workspace --all-features -- -D warnings` がエラーなく通る
- [ ] `cargo test --workspace` (ignored 以外) がすべて pass
- [ ] `tests/ucx_pipelined_allreduce_2proc.rs` が pass (mpiexec で `--ignored` 実行)
- [ ] `tests/ucx_recursive_doubling_2proc.rs` が pass
- [ ] `tests/ucx_allgather_2proc.rs` が pass
- [ ] `tests/ucx_broadcast_2proc.rs` が pass
- [ ] `examples/coll_ucx_pipelined.rs` が動く
- [ ] `examples/coll_adaptive.rs` (アルゴリズム自動選択のデモ) が動く
- [ ] `pluvio_collective/README.md` が更新され、新 API がドキュメントされている
- [ ] PR に「動作確認した環境(OS, UCX version, MPI 実装)」が記載されている

### ベンチマークリポジトリ側

- [ ] `cargo build --release --workspace` が通る
- [ ] `scripts/local/run_all_2proc.sh` で `configs/sweep_quick.yaml` を実行し、CSV と PDF が生成される
- [ ] `bench_runner --help` で適切なヘルプが出る
- [ ] `crates/mpi_baseline` が独立してビルド・実行可能
- [ ] PBS Pro / Slurm 両テンプレートが存在し、`scripts/cluster/README_cluster.md` に「ジョブ提出方法」「期待される実行時間」「失敗時のチェック項目」が書かれている
- [ ] `README.md` に「ローカル 2 プロセスでの動かし方」「クラスタへの投げ方」「結果の見方」が書かれている

### 不要な完成条件 (Phase 3 では満たさなくてよい)

- 4 ノード以上の実行成功(クラスタ予約取得が前提)
- 論文 figure の最終版作成
- 実 DL ライブラリ (PyTorch) との統合

---

## 10. スコープ外 (Phase 4 以降)

- 4-8 ノードでの本格スケーリング測定
- 論文 figure の最終版作成
- アブレーション study (polling discipline / I/O path / pipelining 各次元の独立寄与の分離)
- Tokio / Glommio / Mochi-Margo との比較 (Phase 1+2 と同じ枠組みで)
- GPU 対応 (CUDA awareness, NCCL 比較)
- bf16 / fp16
- MPI 4.0 persistent collective
- Reactor trait の formal verification (Loom)

---

## 11. 開発フロー

### Pluvio 本体側

ブランチ `feat/phase3-pipelined` を切る。

1. **Task 0 (§4) を完了**、`docs/phase3_codebase_notes.md` をコミット
2. AmHeader を 8-byte 化(既存の plain ring と scatter は `micro_chunk = 0` で動かす)。テストが通ることを確認
3. `pipelined_ring.rs` を実装、 `tests/ucx_pipelined_allreduce_2proc.rs` を追加
4. `recursive_doubling.rs` を実装、テスト追加
5. `bruck.rs` (allgather) を実装、`Collective` trait に `allgather` を追加、テスト追加
6. `broadcast.rs` を実装、テスト追加
7. `adaptive.rs` を実装、example 追加
8. README 更新
9. PR 作成

### ベンチマークリポジトリ側 (Pluvio 本体側の Pipelined Ring が出来たら開始)

新規リポジトリ `pluvio_collective_bench` を作る。Pluvio 本体を git submodule として `pluvio/` に置く。

ブランチ `main` で直接作業 (空リポジトリなので)。

1. ディレクトリ構造 (§7.1) を作る
2. `crates/mpi_baseline` を最初に書く(他に依存しないので独立してテストできる)
3. `crates/workloads/src/allreduce_latency.rs`, `allreduce_throughput.rs` を書く
4. `crates/bench_runner` で YAML 設定読み込み + workload dispatch + CSV 出力
5. `scripts/local/run_all_2proc.sh` で動作確認
6. `scripts/plot/` の Python スクリプト群
7. `crates/workloads/src/overlap.rs` (一番論文インパクトが大きいので丁寧に)
8. `crates/workloads/src/dl_microbench.rs`, `stencil.rs`
9. CPU 使用率測定の組み込み
10. PBS / Slurm テンプレート + クラスタ向け README
11. PR (というかマージ済み main にプッシュ)

各 step で `cargo build --release` と `cargo clippy` を回す。

---

## 12. 既知のリスクと対処

### リスク 1: AmRouter の同時 pending 数が多すぎる

→ pipelined ring で max_in_flight = 16, micro_chunks = 8 にすると同時 pending が 128 を超える。`AmRouter::slots: HashMap<RecvKey, Slot>` は問題なくスケールするが、UCX の AM 受信バッファが溢れる可能性がある。

対策: `pluvio_ucx::Worker::am_stream` の内部バッファ深度を確認(Task 0 の調査項目)。必要なら明示的に `ucp_worker_progress` を多めに呼ぶ。

### リスク 2: Recursive doubling の n != 2^k

→ 簡易版として `if !n.is_power_of_two() { return plain_ring().await; }` で逃げる。論文では「2 の冪のときに RD を選ぶ」ポリシーで通せばよい。

### リスク 3: Adaptive ポリシーの threshold 値

→ Phase 3 では仮の値 (small=2KB, large=64KB) を入れておく。評価で fitting して論文書くときに調整。

### リスク 4: ベンチマークの noise task が CPU を食いすぎる

→ Pluvio runtime のタスクは協調的なので yield を入れていれば問題ないが、`volatile_work(noise_work_ns)` が yield を挟まないと collective が走らない。実装時に `tokio::task::yield_now()` (または Pluvio の同等物) を必ず入れる。

### リスク 5: CSV ファイルがロックされて MPI 多プロセスから書けない

→ rank 0 のみが書く。他 rank は in-memory で結果を保持し、最後に rank 0 に send する。あるいは rank ごとに別ファイル (`results/rank{}.csv`) に書いて後で集約する。後者のほうが実装が楽。

### リスク 6: HPC クラスタで cargo build が失敗する

→ クラスタの compute node はインターネット非接続が多い。`cargo vendor` で依存を固めてから git に含めるか、login node で build してから submit する運用にする。 README に明記。

### リスク 7: 開発時 (2 プロセス同一ホスト) と HPC クラスタで動作が違う

→ 同一ホストは loopback で UCX が `tcp` transport にフォールバックすることがある。`UCX_TLS` を明示し、ローカルでも `sm,self` (shared memory) を強制する。クラスタでは `rc,sm,self` (RDMA + shared memory)。

---

## 13. 詰まったら

以下の状況で作業を止めてユーザに質問:

- AmRouter の拡張で既存テストが通らなくなり、設計変更が必要なとき
- Pluvio runtime に sleep / barrier API が無く、ベンチワークロードが書けないとき
- pipelined ring のオーバーラップが期待値より低い (e.g. plain ring と差が出ない) とき
- CPU 使用率測定が `/proc/self/stat` から正しく読めない (権限・フォーマット問題)
- HPC クラスタ向けのモジュール名やパスがユーザ環境と合わない

質問は GitHub issue ではなく、PR description のチェックボックスに書く。

---

## 14. 進め方の優先順位 (claude code 向け)

1. **Pluvio 本体: pipelined ring** (これが出来ないと他が動かない)
2. **ベンチマークリポジトリ: bench_runner skeleton + mpi_baseline + allreduce_latency** (測定基盤を早く立ち上げる)
3. **ベンチマークリポジトリ: allreduce_throughput + overlap + プロット** (主要結果が出る)
4. **Pluvio 本体: recursive doubling + adaptive** (アルゴリズム多様化)
5. **Pluvio 本体: bruck allgather + broadcast** (補助的な操作)
6. **ベンチマークリポジトリ: dl_microbench + stencil** (応用シナリオ)
7. **両方: HPC クラスタ向けスクリプト + 全体 README** (デプロイ準備)

各段階で Phase 1+2 と同じく `cargo build` + `cargo clippy` で warning 0 を維持。

---

## 付録 A: 論文での節構成イメージ (claude code は意識する必要なし)

- §1 Intro: collective async/await が書ける substrate の必要性
- §2 Background: NBC (MPI_Iallreduce), libNBC, MPICH の async progress thread の問題
- §3 Related work: UCC, NCCL, libNBC, Mochi-Margo
- §4 Design:
  - 4.1 Pluvio Reactor trait の復習
  - 4.2 AmRouter の設計
  - 4.3 Pipelined ring の依存グラフ
  - 4.4 Adaptive policy
- §5 Evaluation:
  - 5.1 Setup (testbed)
  - 5.2 Latency sweep
  - 5.3 Throughput sweep
  - 5.4 Overlap measurement (★ 一番強い結果)
  - 5.5 CPU footprint
  - 5.6 DL microbench
  - 5.7 Algorithm crossover (small vs large message)
- §6 Discussion: limitations, future work
- §7 Conclusion

## 付録 B: 参考にすべき外部資料

- libNBC: Hoefler et al., "Implementation and Performance Analysis of Non-Blocking Collective Operations for MPI", SC07
- UCC: <https://github.com/openucx/ucc>
- Rabenseifner: "Optimization of Collective Reduction Operations", ICCS 2004
- Bruck: "Efficient Algorithms for All-to-All Communications in Multiport Message-Passing Systems", IEEE TPDS 1997
- MPICH `MPIR_CVAR_*_INTRA_ALGORITHM` ドキュメント (アルゴリズム選択 threshold の参考値)