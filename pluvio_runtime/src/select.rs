//! Select utilities for combining multiple futures.
//!
//! This module provides the `select` function that polls two futures concurrently
//! and returns the result of whichever completes first, dropping the other.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Result type for `select` operations, indicating which future completed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Either<A, B> {
    /// The first future completed first with its output.
    Left(A),
    /// The second future completed first with its output.
    Right(B),
}

/// A future that polls two futures concurrently and returns the first one to complete.
///
/// When one future completes, the other is dropped (cancelled).
pub struct Select2<F1, F2> {
    future1: Option<F1>,
    future2: Option<F2>,
}

impl<F1, F2> Select2<F1, F2>
where
    F1: Future + Unpin,
    F2: Future + Unpin,
{
    /// Creates a new `Select2` from two futures.
    pub fn new(future1: F1, future2: F2) -> Self {
        Self {
            future1: Some(future1),
            future2: Some(future2),
        }
    }
}

impl<F1, F2> Future for Select2<F1, F2>
where
    F1: Future + Unpin,
    F2: Future + Unpin,
{
    type Output = Either<F1::Output, F2::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Poll future1 first
        if let Some(f1) = self.future1.as_mut() {
            if let Poll::Ready(result) = Pin::new(f1).poll(cx) {
                self.future1 = None;
                self.future2 = None; // Cancel (drop) the other future
                return Poll::Ready(Either::Left(result));
            }
        }

        // Poll future2
        if let Some(f2) = self.future2.as_mut() {
            if let Poll::Ready(result) = Pin::new(f2).poll(cx) {
                self.future1 = None; // Cancel (drop) the other future
                self.future2 = None;
                return Poll::Ready(Either::Right(result));
            }
        }

        Poll::Pending
    }
}

/// Polls two futures concurrently and returns the result of whichever completes first.
///
/// When one future completes, the other is dropped (cancelled).
///
/// # Example
///
/// ```ignore
/// use pluvio_runtime::select::{select, Either};
///
/// async fn example() {
///     let future1 = Box::pin(async { 1 });
///     let future2 = Box::pin(async { 2 });
///
///     match select(future1, future2).await {
///         Either::Left(val) => println!("future1 completed first with {}", val),
///         Either::Right(val) => println!("future2 completed first with {}", val),
///     }
/// }
/// ```
pub fn select<F1, F2>(future1: F1, future2: F2) -> Select2<F1, F2>
where
    F1: Future + Unpin,
    F2: Future + Unpin,
{
    Select2::new(future1, future2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_either_variants() {
        let left: Either<i32, &str> = Either::Left(42);
        let right: Either<i32, &str> = Either::Right("hello");

        assert_eq!(left, Either::Left(42));
        assert_eq!(right, Either::Right("hello"));
    }
}
