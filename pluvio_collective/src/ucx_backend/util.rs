//! Small helpers shared across the UCX collective backends.

/// Split `buf` into multiple disjoint mutable slices described by the
/// `(start, len)` tuples in `ranges`.
///
/// # Safety
/// The caller must guarantee that:
/// - every `start + len` is `<= buf.len()`,
/// - the produced slices are pairwise disjoint (no overlap).
///
/// Used by ring / pipelined-ring allreduce to side-step the borrow checker
/// when several concurrently-running futures each write into a different
/// chunk of the same `&mut [T]`.
#[allow(dead_code)]
pub(crate) unsafe fn split_disjoint_mut<'a, T>(
    buf: &'a mut [T],
    ranges: &[(usize, usize)],
) -> Vec<&'a mut [T]> {
    let base = buf.as_mut_ptr();
    let total = buf.len();
    ranges
        .iter()
        .map(|&(start, len)| {
            debug_assert!(start + len <= total, "range out of bounds");
            // SAFETY: contract delegated to the caller (see doc comment).
            unsafe { std::slice::from_raw_parts_mut(base.add(start), len) }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_disjoint_basic() {
        let mut buf = vec![0u32; 16];
        let parts = unsafe { split_disjoint_mut(&mut buf, &[(0, 4), (4, 8), (12, 4)]) };
        for (i, p) in parts.into_iter().enumerate() {
            for v in p.iter_mut() {
                *v = i as u32;
            }
        }
        assert_eq!(&buf[0..4], &[0, 0, 0, 0]);
        assert_eq!(&buf[4..12], &[1, 1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(&buf[12..16], &[2, 2, 2, 2]);
    }
}
