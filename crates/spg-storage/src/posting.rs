//! Posting lists — the locator sequence stored under one index key.
//!
//! # Why this is not a `Vec<RowLocator>`
//!
//! Index maps are [`crate::PersistentBTreeMap`]s: copy-on-write B-trees
//! whose nodes are shared behind `Arc`. Writing to one walks
//! `Arc::make_mut` down the spine, and a node that is still shared with a
//! reader gets copied — entries and all. `BNode::clone` deep-copies its
//! values, so with a plain `Vec` the copy carries every locator under
//! every key in that node.
//!
//! Round 1028 counted the two halves of that on the mailrs import:
//! 13,194,459 posting-list appends against 16,343 node clones. Eight
//! hundred and seven appends per clone — `Arc::make_mut` finds the node
//! uniquely owned almost always, so a statement copies a node once on
//! first touch and the rest of its appends land in place. The copying is
//! therefore charged per (node, statement), and at roughly half a megabyte
//! a node it came to about 8 GB of the import's 15.2 GB of allocation.
//!
//! An earlier attempt (round 1026) put the whole list behind an `Arc` so
//! the node clone would copy pointers. It measured as a null result, and
//! this shape explains why: the first append then calls `Arc::make_mut` on
//! the LIST, copying it in full. The copy moved from node granularity to
//! list granularity and a statement touches most of a node's lists anyway.
//!
//! # The shape
//!
//! A full prefix of shared blocks plus a short open tail:
//!
//! ```text
//!   frozen: [Arc<[L; 256]>] [Arc<[L; 256]>] [Arc<[L; 256]>]
//!   tail:   [L, L, L]                        <- < BLOCK, owned
//! ```
//!
//! Cloning copies the block POINTERS and the tail. The tail is bounded by
//! `BLOCK`, so a clone costs a few kilobytes however long the list is, and
//! — unlike the `Arc<Vec<_>>` shape — appending afterwards needs no
//! copy at all: the tail is already owned. Pushing past `BLOCK` freezes
//! the tail into a new shared block, which the next clone will only
//! point at.
//!
//! # Cost
//!
//! Appending is amortised `O(1)`; reading is a pointer hop every `BLOCK`
//! locators. An empty list allocates nothing (both `Vec`s start empty), so
//! the per-key overhead against a bare `Vec` is the second `Vec` header —
//! 24 bytes — paid on every distinct key in exchange for the copy bound.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::row_locator::RowLocator;

/// Locators per frozen block.
///
/// This is the clone bound: a copy carries at most this many locators
/// (`BLOCK * 16` bytes ≈ 4 KB) plus one pointer per frozen block. Larger
/// wastes more per clone; smaller spends more pointers and more `Arc`
/// traffic per read.
const BLOCK: usize = 256;

/// The locators stored under one index key. See the module docs for why
/// this is not a `Vec`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PostingList {
    /// Full blocks, shared. Every one holds exactly `BLOCK` locators, so
    /// the length is derivable and is not stored.
    frozen: Vec<Arc<[RowLocator]>>,
    /// The open block: owned, always shorter than `BLOCK`.
    tail: Vec<RowLocator>,
}

impl PostingList {
    /// An empty list. Allocates nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frozen: Vec::new(),
            tail: Vec::new(),
        }
    }

    /// A list holding one locator — the shape every new index key starts
    /// in.
    #[must_use]
    pub fn single(locator: RowLocator) -> Self {
        Self {
            frozen: Vec::new(),
            tail: alloc::vec![locator],
        }
    }

    /// Append one locator.
    pub fn push(&mut self, locator: RowLocator) {
        self.tail.push(locator);
        if self.tail.len() >= BLOCK {
            let full = core::mem::take(&mut self.tail);
            self.frozen.push(Arc::from(full.into_boxed_slice()));
        }
    }

    /// Number of locators.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frozen.len() * BLOCK + self.tail.len()
    }

    /// Whether the list holds no locators.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frozen.is_empty() && self.tail.is_empty()
    }

    /// The locators in insertion order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            list: self,
            block: 0,
            pos: 0,
        }
    }

    /// The locators in insertion order, by value.
    pub fn iter_copied(&self) -> impl Iterator<Item = RowLocator> + '_ {
        self.iter().copied()
    }

    /// The first locator, if any.
    #[must_use]
    pub fn first(&self) -> Option<RowLocator> {
        self.frozen
            .first()
            .and_then(|b| b.first().copied())
            .or_else(|| self.tail.first().copied())
    }

    /// The last locator, if any.
    #[must_use]
    pub fn last(&self) -> Option<RowLocator> {
        self.tail
            .last()
            .copied()
            .or_else(|| self.frozen.last().and_then(|b| b.last().copied()))
    }

    /// Whether `locator` appears in the list.
    #[must_use]
    pub fn contains(&self, locator: RowLocator) -> bool {
        self.iter().any(|l| *l == locator)
    }

    /// Keep only the locators `keep` accepts, rebuilding the blocks.
    ///
    /// Rebuilds rather than edits in place: the frozen blocks are shared,
    /// so dropping from the middle of one would copy it anyway.
    ///
    /// The common call drops nothing, and that case must not allocate — the
    /// insert path prunes a key's dead versions every time its list reaches
    /// a power-of-two length. So the list is tested first and only rebuilt
    /// if something is actually going. `keep` therefore sees a retained
    /// locator TWICE, which is why it is `Fn` and not `FnMut`: a predicate
    /// here has to be pure.
    pub fn retain(&mut self, keep: impl Fn(RowLocator) -> bool) {
        if self.iter().all(|l| keep(*l)) {
            return;
        }
        let kept: Self = self.iter().copied().filter(|l| keep(*l)).collect();
        *self = kept;
    }

    /// Collect into a flat `Vec`, for callers that need contiguity.
    #[must_use]
    pub fn to_vec(&self) -> Vec<RowLocator> {
        let mut out = Vec::with_capacity(self.len());
        out.extend(self.iter().copied());
        out
    }
}

/// Walks a [`PostingList`]'s frozen blocks and then its tail.
///
/// Hand-written rather than a `flat_map(..).chain(..)`: the read paths
/// iterate posting lists per row, and a boxed or deeply-nested adaptor
/// would put an allocation or a layer of indirection there.
#[derive(Debug)]
pub struct Iter<'a> {
    list: &'a PostingList,
    /// Index into `frozen`; equal to its length once the tail is being
    /// walked.
    block: usize,
    /// Offset within the current block (or within the tail).
    pos: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a RowLocator;

    fn next(&mut self) -> Option<&'a RowLocator> {
        while self.block < self.list.frozen.len() {
            let block = &self.list.frozen[self.block];
            if let Some(locator) = block.get(self.pos) {
                self.pos += 1;
                return Some(locator);
            }
            self.block += 1;
            self.pos = 0;
        }
        let locator = self.list.tail.get(self.pos)?;
        self.pos += 1;
        Some(locator)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let seen = self.block * BLOCK + self.pos;
        let left = self.list.len().saturating_sub(seen);
        (left, Some(left))
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl FromIterator<RowLocator> for PostingList {
    fn from_iter<I: IntoIterator<Item = RowLocator>>(iter: I) -> Self {
        let mut out = Self::new();
        for locator in iter {
            out.push(locator);
        }
        out
    }
}

/// Compare against a flat sequence, so a caller holding an expected
/// order does not have to know how the list is blocked internally.
impl PartialEq<[RowLocator]> for PostingList {
    fn eq(&self, other: &[RowLocator]) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a == b)
    }
}

impl<const N: usize> PartialEq<[RowLocator; N]> for PostingList {
    fn eq(&self, other: &[RowLocator; N]) -> bool {
        *self == other[..]
    }
}

impl From<Vec<RowLocator>> for PostingList {
    fn from(v: Vec<RowLocator>) -> Self {
        v.into_iter().collect()
    }
}

impl<'a> IntoIterator for &'a PostingList {
    type Item = &'a RowLocator;
    type IntoIter = alloc::boxed::Box<dyn Iterator<Item = &'a RowLocator> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        alloc::boxed::Box::new(self.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, PostingList};
    use crate::row_locator::RowLocator;

    fn hot(i: usize) -> RowLocator {
        RowLocator::Hot(i)
    }

    #[test]
    fn empty_list_allocates_nothing_and_reads_empty() {
        let list = PostingList::new();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert_eq!(list.iter().count(), 0);
        assert_eq!(list.last(), None);
    }

    #[test]
    fn order_and_length_survive_block_boundaries() {
        // Spans three block boundaries so the frozen/tail split is
        // exercised on both sides of each.
        let n = BLOCK * 3 + 7;
        let list: PostingList = (0..n).map(hot).collect();
        assert_eq!(list.len(), n);
        let read: alloc::vec::Vec<_> = list.iter().copied().collect();
        assert_eq!(read, (0..n).map(hot).collect::<alloc::vec::Vec<_>>());
        assert_eq!(list.last(), Some(hot(n - 1)));
    }

    #[test]
    fn length_is_exact_at_a_block_boundary() {
        // The boundary case the derived length gets wrong if `push`
        // freezes late: at exactly BLOCK the tail must be empty.
        let list: PostingList = (0..BLOCK).map(hot).collect();
        assert_eq!(list.len(), BLOCK);
        assert_eq!(list.iter().count(), BLOCK);
        assert_eq!(list.last(), Some(hot(BLOCK - 1)));
    }

    #[test]
    fn a_clone_does_not_see_later_appends() {
        // The property the whole shape exists for: frozen blocks are
        // shared, so a clone must still be a snapshot.
        let mut original: PostingList = (0..BLOCK * 2).map(hot).collect();
        let snapshot = original.clone();
        original.push(hot(9999));
        assert_eq!(snapshot.len(), BLOCK * 2);
        assert_eq!(original.len(), BLOCK * 2 + 1);
        assert_eq!(snapshot.last(), Some(hot(BLOCK * 2 - 1)));
    }

    #[test]
    fn retain_rebuilds_across_blocks() {
        let mut list: PostingList = (0..BLOCK * 2 + 5).map(hot).collect();
        list.retain(|l| matches!(l, RowLocator::Hot(i) if i % 2 == 0));
        let read: alloc::vec::Vec<_> = list.iter().copied().collect();
        let want: alloc::vec::Vec<_> = (0..BLOCK * 2 + 5).filter(|i| i % 2 == 0).map(hot).collect();
        assert_eq!(read, want);
        assert_eq!(list.len(), want.len());
    }

    #[test]
    fn retain_keeping_everything_leaves_the_list_alone() {
        let list: PostingList = (0..BLOCK + 3).map(hot).collect();
        let mut same = list.clone();
        same.retain(|_| true);
        assert_eq!(same, list);
    }

    #[test]
    fn retain_can_empty_the_list() {
        let mut list: PostingList = (0..BLOCK + 3).map(hot).collect();
        list.retain(|_| false);
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
    }
}
