//! UCX tag-matched 経路で使う 64bit タグのエンコード。
//!
//! AM router 経由ではなく UCX の tag matching に直接乗ることで、
//!  - 受信側で buffer pre-post できる (zero-copy RDMA put/get に乗りやすい)
//!  - HW tag matching (Mellanox ConnectX-5 以降) の恩恵
//!  - per-message HashMap dispatch の overhead が消える
//! ことが期待できる。
//!
//! 64-bit タグは AmHeader と同じ 4 つの軸を 16/16/8/8 bit に詰める。
//! 上位 16 bit は予約 (将来 collective 識別子などに使えるよう 0 で固定)。

#[inline]
pub fn encode_tag(src: usize, step: u16, phase: u8, micro_chunk: u8) -> u64 {
    (src as u64 & 0xffff)
        | ((step as u64) << 16)
        | ((phase as u64) << 32)
        | ((micro_chunk as u64) << 40)
}
