//! Mapping from Rust scalar types to `MPI_Datatype`.

use mpi_sys::{
    MPI_Datatype, RSMPI_DOUBLE as MPI_DOUBLE, RSMPI_FLOAT as MPI_FLOAT,
    RSMPI_UINT32_T as MPI_UINT32_T,
};

/// Plain-old-data scalar type that has a corresponding MPI predefined datatype.
///
/// # Safety
/// Implementors guarantee that `[Self]` is bit-equivalent to `[u8]` of size
/// `len * size_of::<Self>()`, that `dtype()` returns a predefined `MPI_Datatype`
/// matching `Self`, and that values are safe to memcpy.
pub unsafe trait MpiDatatype: Copy + 'static {
    fn dtype() -> MPI_Datatype;
}

unsafe impl MpiDatatype for f32 {
    fn dtype() -> MPI_Datatype {
        unsafe { MPI_FLOAT }
    }
}
unsafe impl MpiDatatype for f64 {
    fn dtype() -> MPI_Datatype {
        unsafe { MPI_DOUBLE }
    }
}
unsafe impl MpiDatatype for u32 {
    fn dtype() -> MPI_Datatype {
        unsafe { MPI_UINT32_T }
    }
}
