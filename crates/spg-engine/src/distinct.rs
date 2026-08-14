//! The seen-set `DISTINCT` probes against.
//!
//! `DISTINCT` keeps a map from a row's normalised hash to the rows already
//! kept under that hash, so a candidate is compared only against rows it
//! could actually equal. What lives in that map is on the per-row path of
//! every `DISTINCT` query.
//!
//! It used to be a `Vec<usize>`. On a column that is already unique the
//! bucket is reached once per row and never twice, so that was one heap
//! allocation per row for a list that only ever held one element. Counting
//! them (r1030, `SELECT DISTINCT k FROM t ORDER BY k` over 400 k unique
//! `k`) priced it exactly:
//!
//! | | allocations per query |
//! |---|---:|
//! | `SELECT k … ORDER BY k` | 800,067 |
//! | `SELECT DISTINCT k … ORDER BY k` | 1,200,087 |
//!
//! One extra per row, +28 ms on a 78 ms query, while the bytes allocated
//! went slightly DOWN — many tiny allocations, not more data.
//!
//! So the first index is held inline and `rest` stays empty, which costs
//! nothing: an empty `Vec` does not allocate. Only a genuine hash collision
//! between two distinct values reaches it.

use alloc::vec::Vec;
use core::num::NonZeroUsize;

/// Indices into the kept-rows buffer that share one normalised hash.
///
/// The first is inline. `NonZeroUsize` holding `index + 1` lets `Option`
/// use its niche, so an empty bucket is 8 bytes and not 16.
#[derive(Default, Debug)]
pub(crate) struct DistinctBucket {
    first: Option<NonZeroUsize>,
    rest: Vec<usize>,
}

impl DistinctBucket {
    /// The kept rows under this hash, in insertion order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.first
            .map(|i| i.get() - 1)
            .into_iter()
            .chain(self.rest.iter().copied())
    }

    /// Record that the row at `index` was kept.
    pub(crate) fn push(&mut self, index: usize) {
        match self.first {
            None => {
                // `index + 1` cannot wrap: it is a position in a buffer
                // that already holds `index` rows.
                self.first = NonZeroUsize::new(index + 1);
            }
            Some(_) => self.rest.push(index),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DistinctBucket;

    #[test]
    fn an_empty_bucket_yields_nothing() {
        let bucket = DistinctBucket::default();
        assert_eq!(bucket.iter().count(), 0);
    }

    #[test]
    fn index_zero_survives_the_offset() {
        // The inline slot stores index + 1, so index 0 is the case that
        // breaks if the offset is dropped on either side.
        let mut bucket = DistinctBucket::default();
        bucket.push(0);
        assert_eq!(bucket.iter().collect::<alloc::vec::Vec<_>>(), [0]);
    }

    #[test]
    fn order_is_insertion_order_across_the_inline_slot() {
        let mut bucket = DistinctBucket::default();
        for i in [7, 0, 42, 3] {
            bucket.push(i);
        }
        assert_eq!(bucket.iter().collect::<alloc::vec::Vec<_>>(), [7, 0, 42, 3]);
    }

    #[test]
    fn the_first_push_does_not_touch_the_overflow() {
        let mut bucket = DistinctBucket::default();
        bucket.push(9);
        assert!(
            bucket.rest.is_empty(),
            "a single-element bucket must not allocate — that is the point"
        );
    }
}
