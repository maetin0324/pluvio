//! Reduction operators for collectives.
//!
//! Each `Op<T>` provides a pure-Rust `apply`/`identity` (used by the UCX ring
//! backend) and, when the `mpi` feature is enabled, the corresponding
//! `MPI_Op` constant (used by the MPI backend).

#[cfg(feature = "mpi")]
use mpi_sys::{
    MPI_Op, RSMPI_BXOR as MPI_BXOR, RSMPI_MAX as MPI_MAX, RSMPI_MIN as MPI_MIN,
    RSMPI_PROD as MPI_PROD, RSMPI_SUM as MPI_SUM,
};

/// A reduction operator that combines two values of `T` associatively.
pub trait Op<T>: Copy + 'static {
    fn apply(a: T, b: T) -> T;
    fn identity() -> T;

    /// In-place バルク reduce: `dst[i] = apply(dst[i], src[i])`。
    /// デフォルトは 8-way chunk_exact ループで、LLVM が AVX2/AVX-512 の
    /// `vaddps` (Sum) や `vmaxps` (Max) を生成しやすい形にしてある。
    /// 個別 Op で更に最適化したい場合は override できる。
    #[inline]
    fn reduce_in_place(dst: &mut [T], src: &[T])
    where
        T: Copy,
    {
        debug_assert_eq!(dst.len(), src.len());
        const LANES: usize = 8;
        let mut chunks_dst = dst.chunks_exact_mut(LANES);
        let mut chunks_src = src.chunks_exact(LANES);
        for (d, s) in (&mut chunks_dst).zip(&mut chunks_src) {
            // 固定長 [T; 8] にすると LLVM が SIMD 8 wide で broadcast load + op + store。
            for i in 0..LANES {
                d[i] = Self::apply(d[i], s[i]);
            }
        }
        let rd = chunks_dst.into_remainder();
        let rs = chunks_src.remainder();
        for (d, s) in rd.iter_mut().zip(rs.iter()) {
            *d = Self::apply(*d, *s);
        }
    }

    /// Return the corresponding `MPI_Op` for the MPI backend.
    /// UCX backend does not consult this.
    #[cfg(feature = "mpi")]
    fn mpi_op() -> MPI_Op;
}

#[derive(Copy, Clone, Debug)]
pub struct Sum;
#[derive(Copy, Clone, Debug)]
pub struct Max;
#[derive(Copy, Clone, Debug)]
pub struct Min;
#[derive(Copy, Clone, Debug)]
pub struct Prod;
/// Bitwise XOR, only defined on integer types. Useful for bit-exact
/// cross-validation between MPI and UCX backends.
#[derive(Copy, Clone, Debug)]
pub struct BitXor;

macro_rules! impl_op_float {
    ($t:ty, $sum_id:expr, $max_id:expr, $min_id:expr, $prod_id:expr, $mpi_t:expr) => {
        impl Op<$t> for Sum {
            #[inline]
            fn apply(a: $t, b: $t) -> $t {
                a + b
            }
            #[inline]
            fn identity() -> $t {
                $sum_id
            }
            #[cfg(feature = "mpi")]
            fn mpi_op() -> MPI_Op {
                unsafe { MPI_SUM }
            }
        }
        impl Op<$t> for Max {
            #[inline]
            fn apply(a: $t, b: $t) -> $t {
                if a > b { a } else { b }
            }
            #[inline]
            fn identity() -> $t {
                $max_id
            }
            #[cfg(feature = "mpi")]
            fn mpi_op() -> MPI_Op {
                unsafe { MPI_MAX }
            }
        }
        impl Op<$t> for Min {
            #[inline]
            fn apply(a: $t, b: $t) -> $t {
                if a < b { a } else { b }
            }
            #[inline]
            fn identity() -> $t {
                $min_id
            }
            #[cfg(feature = "mpi")]
            fn mpi_op() -> MPI_Op {
                unsafe { MPI_MIN }
            }
        }
        impl Op<$t> for Prod {
            #[inline]
            fn apply(a: $t, b: $t) -> $t {
                a * b
            }
            #[inline]
            fn identity() -> $t {
                $prod_id
            }
            #[cfg(feature = "mpi")]
            fn mpi_op() -> MPI_Op {
                unsafe { MPI_PROD }
            }
        }
    };
}

impl_op_float!(f32, 0.0_f32, f32::MIN, f32::MAX, 1.0_f32, ());
impl_op_float!(f64, 0.0_f64, f64::MIN, f64::MAX, 1.0_f64, ());

// Integer specialisations: also provide BitXor for bit-exact tests.
impl Op<u32> for Sum {
    #[inline]
    fn apply(a: u32, b: u32) -> u32 {
        a.wrapping_add(b)
    }
    #[inline]
    fn identity() -> u32 {
        0
    }
    #[cfg(feature = "mpi")]
    fn mpi_op() -> MPI_Op {
        unsafe { MPI_SUM }
    }
}

impl Op<u32> for BitXor {
    #[inline]
    fn apply(a: u32, b: u32) -> u32 {
        a ^ b
    }
    #[inline]
    fn identity() -> u32 {
        0
    }
    #[cfg(feature = "mpi")]
    fn mpi_op() -> MPI_Op {
        unsafe { MPI_BXOR }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_identity_f32() {
        let id = <Sum as Op<f32>>::identity();
        assert_eq!(<Sum as Op<f32>>::apply(id, 3.5), 3.5);
        assert_eq!(<Sum as Op<f32>>::apply(3.5, id), 3.5);
    }

    #[test]
    fn max_min_identity_f64() {
        let id_max = <Max as Op<f64>>::identity();
        let id_min = <Min as Op<f64>>::identity();
        assert_eq!(<Max as Op<f64>>::apply(id_max, 1.0), 1.0);
        assert_eq!(<Min as Op<f64>>::apply(id_min, 1.0), 1.0);
    }

    #[test]
    fn prod_identity_f32() {
        let id = <Prod as Op<f32>>::identity();
        assert_eq!(<Prod as Op<f32>>::apply(id, 2.5), 2.5);
    }

    #[test]
    fn xor_identity_u32() {
        let id = <BitXor as Op<u32>>::identity();
        assert_eq!(<BitXor as Op<u32>>::apply(id, 0xdeadbeef), 0xdeadbeef);
        assert_eq!(<BitXor as Op<u32>>::apply(0xdeadbeef, 0xdeadbeef), 0);
    }
}
