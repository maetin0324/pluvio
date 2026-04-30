# Phase 3 Codebase Notes (Task 0)

Phase 1+2 完了状態 (`feat/collective` マージ済み + scatter 追加) からの差分を埋めるための調査メモ。pipelined ring を書く前に必要な前提を確認する。

## 1. 既存 ring の構造

`pluvio_collective/src/ucx_backend/ring.rs`:

- `ring_allreduce_typed` は `boxed_local()` 経由で `RingAllreduceFuture` に erase した async block を返す。Phase 3 でも同じパターンを採用すれば `Collective::AllreduceFut<'a>` の型を増やさずに済む。
- `run_send_recv` が send / recv を `futures::future::join` で同時 in-flight にしている。pipelined では更にこれを `FuturesUnordered` で多重化する。
- send 側は `bytemuck::cast_slice::<T,u8>(...).to_vec()` で payload を一旦 heap に取って async block にムーブしているので、buf の slice の寿命と send future の寿命が分離される(借用チェッカ的に楽)。
- recv 側は phase 2 (allgather) で send/recv が `buf` の disjoint chunk を指すので `unsafe { std::slice::from_raw_parts_mut(...) }` で raw ポインタ経由に切り出している。pipelined では同じパターンを `split_disjoint_mut` ヘルパに切り出すと再利用しやすい。
- `RecvFuture::Drop` は `registered && !done` の場合 `cancel_pending(key)` を呼ぶ — cancellation 安全性は確保済み。

## 2. `pluvio_ucx::Endpoint::am_send` の挙動

`pluvio_ucx/src/worker/am.rs:37-67` → `async-ucx/src/ucp/endpoint/am.rs:578-655`:

- 内部は `ucp_am_send_nbx` 呼び出し。返り値 `status` で 3 分岐:
  - `null`: **インラインで完了**。Future は即 Ready。
  - `UCS_PTR_IS_PTR(status)`: `request_handle(status, poll_normal).await` で Request 完了まで待機。waker は UCX 側のコールバックで起動。
  - エラーポインタ: 即 Err。
- `AmProto::Eager` を渡すと `UCP_AM_SEND_FLAG_EAGER` がセットされ、UCX はヘッダ+ペイロードを内部バッファにコピーしてから帰る → Eager だとほぼ常にインライン完了。
- `AmProto::Rndv` を渡すと `UCP_AM_SEND_FLAG_RNDV` がセットされ、ハンドシェイク後 RDMA で送る → Future は Pending を経由する。
- **Phase 3 重要点**: Eager は in-flight 数が増えても callback を介さない分軽い。pipelined ring の micro-chunk が小さい時は Eager 一択。大きい時(MTU 超え)は async-ucx 側で eager 上限を超えると自動的に rndv にフォールバックするので `proto: None` でもよいが、明示的に分岐させたいなら `PipelineConfig` で選べるようにする。

`am_send` 内では `worker.activate()` だけ呼んで `worker.deactivate()` は呼ばない(コメント: 「messages may still be in flight in UCX layer」)。pipelined で大量の send を発行する間 active counter が高止まりするが、reactor 側で問題ない。

## 3. AmRouter の同時 pending スケーラビリティ

`pluvio_collective/src/ucx_backend/am_router.rs`:

- `slots: RefCell<HashMap<RecvKey, Slot>>` — HashMap は 100〜数千エントリを扱うのに何の問題もない。
- 現状の `RecvKey = (src: u16, step: u16, phase: u8)`。Phase 1+2 では plain ring の各 step が直列なので衝突しない。pipelined では同じ `(src, step)` で複数 micro_chunk が同時 in-flight になるため **キー衝突が起きる**。

→ **`AmHeader` を 8 byte に拡張し `micro_chunk: u8` を追加する**(プラン §5.4 通り)。`RecvKey` 側にも同フィールドを追加する。既存の plain ring / scatter は `micro_chunk = 0` 固定で動かす。

`AmHeader::BYTES` を 6 → 8 に変える際は、テストや encode/decode の固定長配列を全て更新する必要がある。

## 4. AmStream 内部バッファ

`async-ucx/src/ucp/endpoint/am.rs:408-461` の `AmStreamInner`:

```rust
pub(crate) struct AmStreamInner {
    id: u16,
    msgs: SegQueue<RawMsg>,            // crossbeam の lock-free unbounded queue
    notify: Notify,
    unregistered: AtomicBool,
}
```

- `msgs` は **crossbeam の SegQueue (unbounded)**。コールバックが UCX progress 中に push、`wait_msg` が pop。バックプレッシャは UCX 側のフロー制御任せ。
- AM データ自体は header (Vec<u8>, owned) と payload (Eager: Vec<u8> owned / Rndv: descriptor) を保持。Rndv の場合のみ `ucp_am_recv_data_release` を `AmMsg::drop` で呼び戻す。

→ pipelined で送信側が大量の AM を流しても、受信側 dispatcher が次々 pop すれば溜まらない。dispatcher の `recv_data_single().await` は Eager なら同期コピー(早い)、Rndv なら別レイテンシ。**dispatcher は1個しか走っていないので、Rndv を多用すると直列化される**ので、Eager 推奨(pipelined ring の micro_chunk は通常 KiB オーダーなので Eager で問題ない)。

## 5. plan の API 設計に関する判断メモ

### 5.1 `Collective` trait の `AllreduceFut<'a>` 型

`Collective::AllreduceFut<'a>` は GAT。MPI 側は `AllreduceFuture<'a, T>`、UCX 側は `RingAllreduceFuture<'a, T>` (= `Pin<Box<dyn Future + 'a>>` のラッパ)。

pipelined / recursive doubling / adaptive を全部 trait に流すなら:
- A: 各アルゴリズムを別メソッド (`allreduce_pipelined`, etc.) として `UcxCommunicator` に追加し、`Collective::allreduce` のデフォルトは plain ring のまま。アダプティブ選択は `AdaptiveCommunicator` ラッパで実装。
- B: `Collective::allreduce` が adaptive 選択する。

→ プランは **A (明示メソッド + AdaptiveCommunicator)** を推奨。型の単純さからも A を採用する。

### 5.2 AdaptiveCommunicator の `allreduce` 戻り型

内部で plain / pipelined / RD を切り替えるなら戻り型は erase が必要 → `Pin<Box<dyn Future<Output=Result<...>> + 'a>>` 一択。`UcxCommunicator::allreduce` (plain ring) も既に erase 済みなのでオーバーヘッドは同じオーダー。

## 6. Adaptive policy デフォルト値

参考にできる文献値:
- MPICH 4 の `MPIR_CVAR_ALLREDUCE_SHORT_MSG_SIZE`: 2048 bytes (2 KiB)
- OpenMPI の coll/tuned: 短メッセージ recursive doubling、中 ring、長 Rabenseifner (我々は Rabenseifner は実装しない)

Phase 3 デフォルト: `small_threshold = 2 * 1024`, `large_threshold = 64 * 1024`。後段のベンチで fitting する。

## 7. 結論 (実装に向けたチェックリスト)

- [x] Pipelined ring の micro_chunk 同時 in-flight に備えて、`AmHeader` を 8 byte (`micro_chunk: u8` 追加) に拡張する
- [x] plain ring と scatter は `micro_chunk = 0` 固定で動かす(後方互換は不要、全箇所 8-byte に統一)
- [x] pipelined ring は `Eager` プロトコル前提で実装(必要なら `PipelineConfig` で `AmProto` を選べるようにする)
- [x] AdaptiveCommunicator は `Pin<Box<dyn Future>>` で型 erase
- [x] `split_disjoint_mut` ヘルパを `ucx_backend::util` に置いて、ring/pipelined/scatter で共有
- [x] `AmRouter` の `pending_count()` 既存テストヘルパを残す(pipelined の単体テストで使う)
