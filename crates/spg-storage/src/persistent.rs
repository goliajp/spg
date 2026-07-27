//! Persistent (structural-sharing) vector — the v4.38 building block for the
//! v4.39 cheap-`Catalog::clone` migration.
//!
//! `PersistentVec<T>` is a Bitmapped Vector Trie (Clojure persistent vector
//! shape): 32-way branching trie with a `tail` buffer at the open end. Every
//! mutating operation produces a new handle that shares interior nodes with
//! the old handle via `Arc`. `Clone` is `O(1)`; `push` and `get` are
//! `O(log₃₂ N)`; a `CoW` path touches only the spine of the affected leaf.
//!
//! Hard rules (do not relax in later milestones):
//! - `no_std` compatible (`alloc::sync::Arc`, `alloc::vec::Vec`).
//! - Zero `unsafe`. Workspace lint `unsafe_code = "deny"` stays in force here.
//! - Zero external deps. Pure std + `alloc`.
//!
//! Layout:
//! - `root: Arc<Node<T>>` — the persistent trie. `Node::Internal(Vec<Arc<Node>>)`
//!   for non-leaf levels, `Node::Leaf(Vec<T>)` for the bottom.
//! - `tail: Arc<Vec<T>>` — the open-end buffer (≤ 32 elements). Lives outside
//!   the trie so `push` to a non-full tail avoids walking the spine.
//! - `len: usize` — total element count (`trie_size + tail.len()`).
//! - `shift: u32` — distance from the root to the leaf level, in bits, in
//!   multiples of `SHIFT`. An empty PV has `shift = SHIFT` and an empty root
//!   so the first incorporate doesn't have to special-case the root type.
//!
//! Invariants (debug-asserted in hot paths):
//! - `tail.len() ≤ BRANCH`.
//! - When `tail.len() == BRANCH` we incorporate it into the trie before the
//!   next push (so post-condition is `tail.len() < BRANCH`, except briefly in
//!   the middle of `push`).
//! - `shift` is always a multiple of `SHIFT` and ≥ `SHIFT`.
//! - `trie_size = len - tail.len()` always fits in `1 << (shift + SHIFT)`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Index;

const SHIFT: u32 = 5;
const BRANCH: usize = 1 << SHIFT; // 32
const MASK: usize = BRANCH - 1; // 0x1F

// `Clone` (v5.5.0) backs `Arc::make_mut` in `get_mut_in_trie`: cloning an
// `Internal` only bumps its children's `Arc`s (shallow), cloning a `Leaf`
// copies its ≤ BRANCH elements — exactly the path-copy a shared spine needs,
// matching what `set_in_trie` does by hand.
#[derive(Debug, Clone)]
enum Node<T> {
    Internal(Vec<Arc<Node<T>>>),
    Leaf(Vec<T>),
}

/// A persistent vector with structural sharing. `Clone` is O(1) (bumps the
/// root `Arc`); `push` is amortised O(log₃₂ N) and only allocates fresh nodes
/// along the spine from the root to the affected leaf.
#[derive(Debug)]
pub struct PersistentVec<T> {
    root: Arc<Node<T>>,
    tail: Arc<Vec<T>>,
    len: usize,
    shift: u32,
}

impl<T> Default for PersistentVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for PersistentVec<T> {
    /// O(1) — only `Arc` bumps, no element copy. This is the whole reason PV
    /// exists in v4.38; `Catalog::clone` in v4.39 inherits the property.
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            tail: self.tail.clone(),
            len: self.len,
            shift: self.shift,
        }
    }
}

/// Element-wise equality: two PVs are equal iff they yield the same elements
/// in the same order. Independent of internal trie shape — two PVs built via
/// different push / set sequences with the same end state still compare
/// equal. Used by `Catalog::serialize` round-trip tests in v4.39+.
impl<T: PartialEq> PartialEq for PersistentVec<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Eq> Eq for PersistentVec<T> {}

impl<T> PersistentVec<T> {
    /// Empty vector. Allocates one empty `Internal` root and one empty `tail`
    /// `Vec`; both are shared across every empty PV via `Arc::clone` once the
    /// first one is built. The shape matches a `shift = SHIFT` trie so the
    /// incorporate path never has to grow the root type.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(Node::Internal(Vec::new())),
            tail: Arc::new(Vec::new()),
            len: 0,
            shift: SHIFT,
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// O(log₃₂ N). `None` for out-of-bounds. Returned reference is valid for
    /// the lifetime of `&self`; structural sharing means the borrow is
    /// independent of any other handle that shares the same spine.
    pub fn get(&self, i: usize) -> Option<&T> {
        let (run, off) = self.run_at(i)?;
        run.get(off)
    }

    /// v7.39 (round 562) — the contiguous run holding `i`, with the index
    /// `i` sits at inside it, so a caller reading ascending indices can
    /// keep the run and descend once per leaf instead of once per element.
    ///
    /// This is what `iter` already does; `run_at`'s own comment says so.
    /// It was private, so a caller that reads BY INDEX — an index-only
    /// scan checking one header per matching row — had no way to say it,
    /// and paid a descent per row for elements 32 to a leaf.
    ///
    /// Returns `(start, run)`: `run[i - start]` is element `i`, and the
    /// run covers `start .. start + run.len()`.
    pub fn run_containing(&self, i: usize) -> Option<(usize, &[T])> {
        let (run, off) = self.run_at(i)?;
        Some((i - off, run))
    }

    /// v7.39 (round 567) — a cursor that holds the run it last descended
    /// to, for a caller reading many elements by ascending index.
    ///
    /// Indexing is `O(log₃₂ N)` — four dependent loads over 500k
    /// elements — and a scan that reads every row pays it every row. A
    /// leaf holds 32, so keeping it between reads makes that one descent
    /// per 32. Ask for a scattered index and it descends, exactly as
    /// `get` would.
    pub const fn run_cursor(&self) -> RunCursor<'_, T> {
        RunCursor {
            vec: self,
            run: None,
        }
    }

    /// The contiguous run of elements holding index `i`, plus `i`'s offset
    /// inside it. One trie descent serves the whole run, which is what lets
    /// `iter` walk a leaf at a time instead of descending per element.
    ///
    /// This is `get`'s arithmetic, factored out: the trie region indexes a
    /// leaf by `i & MASK`, the tail by `i - trie_size`.
    fn run_at(&self, i: usize) -> Option<(&[T], usize)> {
        if i >= self.len {
            return None;
        }
        let trie_size = self.len - self.tail.len();
        if i >= trie_size {
            return Some((&self.tail, i - trie_size));
        }
        let mut node: &Arc<Node<T>> = &self.root;
        let mut shift = self.shift;
        loop {
            match &**node {
                Node::Leaf(elems) => return Some((elems, i & MASK)),
                Node::Internal(children) => {
                    let sub_idx = (i >> shift) & MASK;
                    node = children.get(sub_idx)?;
                    shift = shift.saturating_sub(SHIFT);
                }
            }
        }
    }

    /// Sequential iterator, walking a leaf at a time.
    ///
    /// v7.39 (round 486) — this used to call `get` per element, so every
    /// scan in the engine paid a full trie descent (a chain of `Arc`
    /// dereferences) for each row it read. The v4.38 comment here said
    /// "v4.39 / v4.40 will profile and upgrade if iter shows up as the
    /// bottleneck"; it showed up — `is_row_visible` plus the scan's own
    /// row reads were 17 % of `big_in`'s profile, both of them descents.
    /// One descent now serves up to `BRANCH` elements.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            pv: self,
            pos: 0,
            run: &[],
            off: 0,
        }
    }
}

impl<'a, T> IntoIterator for &'a PersistentVec<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// `pv[i]` indexing, matching `Vec<T>::index`'s contract: panics on
/// out-of-bounds. v4.39 lets `table.rows[i]` work unchanged on the new
/// PV-backed `Table` for the price of one extra `O(log₃₂ N)` walk per
/// lookup (vs Vec's O(1)). Callers in a hot loop should hoist the trie
/// walk where possible (`let row = pv.get(i)?;`) instead of re-indexing.
impl<T> Index<usize> for PersistentVec<T> {
    type Output = T;
    fn index(&self, i: usize) -> &T {
        self.get(i).expect("PersistentVec index out of bounds")
    }
}

impl<T: Clone> PersistentVec<T> {
    /// `O(log₃₂ N)` path-copy push. Returns a new handle; `self` is untouched
    /// (structural sharing means the old handle and the new one share every
    /// internal node except the spine to the newly written tail / leaf).
    #[must_use]
    pub fn push(&self, x: T) -> Self {
        // Fast path: tail still has room.
        if self.tail.len() < BRANCH {
            let mut new_tail = (*self.tail).clone();
            new_tail.push(x);
            return Self {
                root: self.root.clone(),
                tail: Arc::new(new_tail),
                len: self.len + 1,
                shift: self.shift,
            };
        }
        // Slow path: tail is full → incorporate it into the trie as a new
        // Leaf, then start a fresh tail with `x`.
        let leaf: Arc<Node<T>> = Arc::new(Node::Leaf((*self.tail).clone()));
        let old_trie_size = self.len - BRANCH; // tail.len() == BRANCH here
        let trie_capacity: usize = 1usize << (self.shift + SHIFT);
        let needs_grow = old_trie_size + BRANCH > trie_capacity;
        let (new_root, new_shift) = if needs_grow {
            // Root overflow: wrap the old root and a brand-new branch (carrying
            // the new leaf) under a fresh top-level Internal. The new branch
            // sits at the same depth the old root sat at, so it needs
            // `old_shift / SHIFT` layers of `Internal` above the leaf.
            let internal_levels_above_leaf = self.shift / SHIFT;
            let new_branch = new_path(internal_levels_above_leaf, leaf);
            let new_root = Arc::new(Node::Internal(alloc::vec![self.root.clone(), new_branch]));
            (new_root, self.shift + SHIFT)
        } else {
            (
                push_leaf_into_node(&self.root, self.shift, old_trie_size, leaf),
                self.shift,
            )
        };
        Self {
            root: new_root,
            tail: Arc::new(alloc::vec![x]),
            len: self.len + 1,
            shift: new_shift,
        }
    }

    /// `O(1)` amortized — transient in-place push. v4.39.1 perf path for the
    /// `Table::insert` hot loop (and any other streaming caller that holds a
    /// `&mut PersistentVec`). Uses `Arc::make_mut` on the tail buffer: when
    /// the tail's `Arc` is uniquely owned (the common case), this mutates
    /// in place — same cost as `Vec::push`. If a cloned handle is outstanding
    /// (e.g. inside a TX wrap holding a Catalog snapshot), the tail is path-
    /// copied just like `push` and the snapshot is unaffected. Either way,
    /// callers observe the same end state as `self = self.push(x)`.
    pub fn push_mut(&mut self, x: T) {
        if self.tail.len() < BRANCH {
            // Fast path: room in tail, mutate in place when uniquely owned.
            let tail = Arc::make_mut(&mut self.tail);
            tail.push(x);
            self.len += 1;
            return;
        }
        // Slow path: tail full → incorporate into trie, then start a fresh
        // tail with [x]. Take ownership of the tail Arc to reuse its Vec
        // when uniquely owned; the placeholder replacement makes self.tail
        // the fresh `[x]` buffer.
        let old_tail_arc = core::mem::replace(&mut self.tail, Arc::new(alloc::vec![x]));
        let old_tail_vec: Vec<T> =
            Arc::try_unwrap(old_tail_arc).unwrap_or_else(|arc| (*arc).clone());
        let leaf: Arc<Node<T>> = Arc::new(Node::Leaf(old_tail_vec));
        let old_trie_size = self.len - BRANCH;
        let trie_capacity: usize = 1usize << (self.shift + SHIFT);
        let needs_grow = old_trie_size + BRANCH > trie_capacity;
        if needs_grow {
            let internal_levels = self.shift / SHIFT;
            let new_branch = new_path(internal_levels, leaf);
            self.root = Arc::new(Node::Internal(alloc::vec![self.root.clone(), new_branch]));
            self.shift += SHIFT;
        } else {
            self.root = push_leaf_into_node(&self.root, self.shift, old_trie_size, leaf);
        }
        self.len += 1;
    }

    /// `O(log₃₂ N)` path-copy set. `None` for out-of-bounds (matches `get`).
    /// Result shares every node except the spine to the rewritten cell.
    #[must_use]
    pub fn set(&self, i: usize, x: T) -> Option<Self> {
        if i >= self.len {
            return None;
        }
        let trie_size = self.len - self.tail.len();
        if i >= trie_size {
            let mut new_tail: Vec<T> = (*self.tail).clone();
            new_tail[i - trie_size] = x;
            return Some(Self {
                root: self.root.clone(),
                tail: Arc::new(new_tail),
                len: self.len,
                shift: self.shift,
            });
        }
        let new_root = set_in_trie(&self.root, self.shift, i, x);
        Some(Self {
            root: new_root,
            tail: self.tail.clone(),
            len: self.len,
            shift: self.shift,
        })
    }

    /// `O(log₃₂ N)` transient-mut access — the read-side analogue of
    /// `push_mut` (v5.5.0). Walks the spine with `Arc::make_mut`: when every
    /// node along the path is uniquely owned (the common streaming case) the
    /// walk mutates in place at the same cost as `Vec::get_mut`. If a cloned
    /// handle shares the spine (e.g. a `Catalog` snapshot held by an open TX),
    /// the touched nodes are path-copied — the snapshot keeps its old value
    /// and only this handle observes the mutation, exactly like `set`. `None`
    /// for out-of-bounds (matches `get` / `set`).
    ///
    /// Introduced for the v5.5 HNSW `NswGraph` switch to PV-backed layers: the
    /// insert path needs in-place edits to a node's neighbour list
    /// (`layers[l].get_mut(node)`) without the `set`-then-write-back round trip
    /// and its extra path-copy.
    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i >= self.len {
            return None;
        }
        let trie_size = self.len - self.tail.len();
        if i >= trie_size {
            let tail = Arc::make_mut(&mut self.tail);
            return tail.get_mut(i - trie_size);
        }
        get_mut_in_trie(&mut self.root, self.shift, i)
    }
}

/// Push a freshly-built `Leaf` into the trie at trie-position `trie_index`.
/// Assumes the caller has already verified `trie_index < trie_capacity` (i.e.
/// `needs_grow == false`). `shift` is the shift at `node`; recursion drops
/// it by `SHIFT` per layer.
fn push_leaf_into_node<T: Clone>(
    node: &Arc<Node<T>>,
    shift: u32,
    trie_index: usize,
    leaf: Arc<Node<T>>,
) -> Arc<Node<T>> {
    let sub_idx = (trie_index >> shift) & MASK;
    let Node::Internal(children) = &**node else {
        // Bottom-of-trie is `Leaf`; we never recurse below `shift == SHIFT`.
        // Reaching a `Leaf` here would be a shift-bookkeeping bug.
        debug_assert!(false, "push_leaf_into_node hit a Leaf — shift bug");
        return node.clone();
    };
    let mut new_children: Vec<Arc<Node<T>>> = children.clone();
    if shift == SHIFT {
        // Next layer down is the Leaf layer — drop the new leaf in at the
        // open slot. Leaves are inserted in trie-index order, so `sub_idx`
        // is always either an existing index (replace — shouldn't happen
        // during push, only during set) or one past the end (append).
        debug_assert!(
            sub_idx == new_children.len(),
            "leaves are pushed sequentially; sub_idx {} != next slot {}",
            sub_idx,
            new_children.len()
        );
        new_children.push(leaf);
    } else {
        let child: Arc<Node<T>> = if sub_idx < new_children.len() {
            push_leaf_into_node(&new_children[sub_idx], shift - SHIFT, trie_index, leaf)
        } else {
            // Fresh branch: wrap the leaf in enough Internal layers to land
            // at the leaf level under this node's child.
            let internal_levels_above_leaf = (shift / SHIFT) - 1;
            new_path(internal_levels_above_leaf, leaf)
        };
        if sub_idx < new_children.len() {
            new_children[sub_idx] = child;
        } else {
            new_children.push(child);
        }
    }
    Arc::new(Node::Internal(new_children))
}

/// Build a chain of `internal_levels` `Internal` nodes wrapping `leaf`. With
/// `internal_levels == 0` the leaf is returned as-is.
fn new_path<T>(internal_levels: u32, leaf: Arc<Node<T>>) -> Arc<Node<T>> {
    let mut node = leaf;
    for _ in 0..internal_levels {
        node = Arc::new(Node::Internal(alloc::vec![node]));
    }
    node
}

/// Path-copy `set` walk. Returns a fresh `Arc<Node>` along the spine; every
/// other node is shared via `Arc::clone`.
fn set_in_trie<T: Clone>(node: &Arc<Node<T>>, shift: u32, i: usize, x: T) -> Arc<Node<T>> {
    match &**node {
        Node::Leaf(elems) => {
            let mut new_elems = elems.clone();
            new_elems[i & MASK] = x;
            Arc::new(Node::Leaf(new_elems))
        }
        Node::Internal(children) => {
            let sub_idx = (i >> shift) & MASK;
            let new_child = set_in_trie(&children[sub_idx], shift - SHIFT, i, x);
            let mut new_children = children.clone();
            new_children[sub_idx] = new_child;
            Arc::new(Node::Internal(new_children))
        }
    }
}

/// Copy-on-write `get_mut` walk (v5.5.0). `Arc::make_mut` clones a node only
/// when it's shared; a uniquely-owned spine is walked in place. Mirrors
/// `set_in_trie` but hands back a `&mut` to the located cell instead of
/// rewriting it, so the caller can mutate the element directly.
fn get_mut_in_trie<T: Clone>(node: &mut Arc<Node<T>>, shift: u32, i: usize) -> Option<&mut T> {
    match Arc::make_mut(node) {
        Node::Leaf(elems) => elems.get_mut(i & MASK),
        Node::Internal(children) => {
            let sub_idx = (i >> shift) & MASK;
            let child = children.get_mut(sub_idx)?;
            get_mut_in_trie(child, shift - SHIFT, i)
        }
    }
}

/// Sequential `&T` iterator. v4.38 implementation is `get(i)`-driven — simple
/// and correct, but O(N log N) over the whole vector. Profile in v4.39 /
/// v4.40 and upgrade if it shows up in flamegraphs.
#[derive(Debug)]
pub struct Iter<'a, T> {
    pv: &'a PersistentVec<T>,
    pos: usize,
    /// The run `pos` currently sits in, and how far into it we are.
    /// Empty (with `off == 0`) means "descend on the next call".
    run: &'a [T],
    off: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;
    fn next(&mut self) -> Option<&'a T> {
        if self.off == self.run.len() {
            let (run, off) = self.pv.run_at(self.pos)?;
            self.run = run;
            self.off = off;
        }
        let v = self.run.get(self.off)?;
        self.off += 1;
        self.pos += 1;
        Some(v)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.pv.len.saturating_sub(self.pos);
        (remaining, Some(remaining))
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}

#[cfg(test)]
impl<T> PersistentVec<T> {
    /// Test-only: do two handles share the same root + tail `Arc` — i.e. did
    /// `clone` bump pointers rather than copy elements? Used by v5.5.0's
    /// `nsw_clone_is_o1` to prove `NswGraph::clone` is O(1) structural sharing,
    /// not an O(N) element copy.
    pub(crate) fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root) && Arc::ptr_eq(&self.tail, &other.tail)
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::needless_range_loop,
    clippy::items_after_statements,
    clippy::manual_range_patterns,
    clippy::unreadable_literal,
    clippy::similar_names
)]
mod tests {
    use super::*;

    /// v7.39 (round 486) — `iter` walks a leaf at a time now instead of
    /// calling `get` per element. The two must stay indistinguishable, so
    /// this checks them against each other at every length that puts a
    /// boundary somewhere interesting: inside the tail, exactly on a leaf
    /// edge, one past it, and deep enough to need a second trie level.
    #[test]
    fn iter_agrees_with_get_at_every_boundary() {
        for n in [
            0usize, 1, 2, 31, 32, 33, 63, 64, 65, 1023, 1024, 1025, 1057, 2000,
        ] {
            let mut pv: PersistentVec<usize> = PersistentVec::new();
            for i in 0..n {
                pv = pv.push(i * 7 + 1);
            }
            let by_index: Vec<usize> = (0..n).map(|i| *pv.get(i).unwrap()).collect();
            let by_iter: Vec<usize> = pv.iter().copied().collect();
            assert_eq!(by_iter, by_index, "n = {n}");
            assert_eq!(pv.iter().count(), n, "n = {n}");
            assert_eq!(pv.iter().len(), n, "ExactSizeIterator, n = {n}");
        }
    }

    /// A partially-consumed walk must keep going from where it stopped,
    /// including across the leaf boundary it is sitting on.
    #[test]
    fn iter_resumes_across_a_leaf_boundary() {
        let mut pv: PersistentVec<usize> = PersistentVec::new();
        for i in 0..200 {
            pv = pv.push(i);
        }
        let mut it = pv.iter();
        let head: Vec<usize> = it.by_ref().take(32).copied().collect();
        assert_eq!(head, (0..32).collect::<Vec<_>>());
        assert_eq!(it.len(), 168);
        let tail: Vec<usize> = it.copied().collect();
        assert_eq!(tail, (32..200).collect::<Vec<_>>());
    }

    /// The walk reads the handle it was made from, not whatever the
    /// structural sharing produced later.
    #[test]
    fn iter_sees_its_own_handles_contents() {
        let mut pv: PersistentVec<usize> = PersistentVec::new();
        for i in 0..40 {
            pv = pv.push(i);
        }
        let older = pv.clone();
        let newer = pv.push(999).set(0, 111).unwrap();
        assert_eq!(older.iter().copied().collect::<Vec<_>>(), (0..40).collect::<Vec<_>>());
        let seen: Vec<usize> = newer.iter().copied().collect();
        assert_eq!(seen.len(), 41);
        assert_eq!(seen[0], 111);
        assert_eq!(seen[40], 999);
    }

    #[test]
    fn empty_vec_is_empty() {
        let pv: PersistentVec<u64> = PersistentVec::new();
        assert_eq!(pv.len(), 0);
        assert!(pv.is_empty());
        assert!(pv.get(0).is_none());
    }

    #[test]
    fn push_single_fits_in_tail() {
        let pv: PersistentVec<u64> = PersistentVec::new().push(42);
        assert_eq!(pv.len(), 1);
        assert_eq!(pv.get(0), Some(&42));
        assert!(pv.get(1).is_none());
    }

    #[test]
    fn push_fills_tail_then_incorporates() {
        // 32 elements all sit in tail; 33rd triggers the first incorporate.
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        for i in 0..40_u64 {
            pv = pv.push(i);
        }
        for i in 0..40_u64 {
            assert_eq!(pv.get(i as usize), Some(&i), "mismatch at {i}");
        }
        assert!(pv.get(40).is_none());
    }

    #[test]
    fn push_crosses_root_overflow_boundary() {
        // Crossing 1024 forces the first root grow (`shift` 5 → 10).
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        for i in 0..1100_u64 {
            pv = pv.push(i);
        }
        for i in 0..1100_u64 {
            assert_eq!(pv.get(i as usize), Some(&i), "mismatch at {i}");
        }
    }

    #[test]
    fn push_crosses_second_grow_boundary() {
        // 32_768 forces the second root grow (`shift` 10 → 15). Verifies the
        // recursion in `push_leaf_into_node` handles a 3-deep trie.
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        for i in 0..33_000_u64 {
            pv = pv.push(i);
        }
        // Spot-check a handful — the full 33k loop is too slow under cargo
        // test default mode; the 100K fuzz oracle covers thorough coverage.
        let probes = [0_usize, 1, 31, 32, 1023, 1024, 1056, 32_767, 32_768, 32_999];
        for &p in &probes {
            assert_eq!(pv.get(p), Some(&(p as u64)), "mismatch at {p}");
        }
        assert!(pv.get(33_000).is_none());
    }

    #[test]
    fn clone_then_push_preserves_original() {
        // The whole point of PV: pushing onto a clone must not mutate the
        // original handle.
        let mut a: PersistentVec<u64> = PersistentVec::new();
        for i in 0..50_u64 {
            a = a.push(i);
        }
        let b = a.clone();
        let b = b.push(999);
        assert_eq!(a.len(), 50);
        assert_eq!(b.len(), 51);
        assert_eq!(a.get(50), None);
        assert_eq!(b.get(50), Some(&999));
        // First 50 elements are visible from both handles.
        for i in 0..50_usize {
            assert_eq!(a.get(i), Some(&(i as u64)));
            assert_eq!(b.get(i), Some(&(i as u64)));
        }
    }

    #[test]
    fn set_rewrites_element_in_tail() {
        let pv: PersistentVec<u64> = PersistentVec::new()
            .push(10)
            .push(20)
            .push(30)
            .set(1, 200)
            .unwrap();
        assert_eq!(pv.get(0), Some(&10));
        assert_eq!(pv.get(1), Some(&200));
        assert_eq!(pv.get(2), Some(&30));
    }

    #[test]
    fn set_rewrites_element_in_trie() {
        // Need ≥ 33 elements so that position 0 lives in the trie, not tail.
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        for i in 0..40_u64 {
            pv = pv.push(i);
        }
        let pv2 = pv.set(0, 9999).unwrap();
        assert_eq!(pv2.get(0), Some(&9999));
        assert_eq!(pv.get(0), Some(&0), "set must not mutate original");
        assert_eq!(pv2.get(39), Some(&39));
    }

    #[test]
    fn set_out_of_bounds_is_none() {
        let pv: PersistentVec<u64> = PersistentVec::new().push(1);
        assert!(pv.set(5, 99).is_none());
    }

    #[test]
    fn iter_matches_get_for_full_walk() {
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        for i in 0..200_u64 {
            pv = pv.push(i * 7);
        }
        let via_iter: Vec<u64> = pv.iter().copied().collect();
        let via_get: Vec<u64> = (0..pv.len()).map(|i| *pv.get(i).unwrap()).collect();
        assert_eq!(via_iter, via_get);
        assert_eq!(via_iter.len(), 200);
        assert_eq!(via_iter[199], 199 * 7);
    }

    #[test]
    fn iter_size_hint_exact() {
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        for i in 0..15_u64 {
            pv = pv.push(i);
        }
        let it = pv.iter();
        assert_eq!(it.size_hint(), (15, Some(15)));
        assert_eq!(it.count(), 15);
    }

    /// SplitMix-style PRNG so the fuzz oracle is reproducible without pulling
    /// `rand` in. Same mixer the NSW level assignment uses upstream.
    struct Splitmix(u64);
    impl Splitmix {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = self.0;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        }
    }

    /// Random `push` / `set` / `get` operation sequence ≥ 100K steps mirrored
    /// against the std `Vec<u64>`. Confirms PV's mutating ops match the
    /// canonical ground-truth semantics across every BVT branch / tail
    /// boundary / root overflow.
    #[test]
    fn fuzz_oracle_against_vec_u64() {
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        let mut oracle: Vec<u64> = Vec::new();
        let mut rng = Splitmix::new(0xC0FFEE_u64);
        const STEPS: usize = 100_000;
        for step in 0..STEPS {
            let r = rng.next();
            // Bias toward push so we actually grow the trie past the second
            // boundary (33k+ in ~80k pushes).
            let op = r % 4; // 0..2 push, 3 set
            match (op, oracle.len()) {
                (0 | 1 | 2, _) | (_, 0) => {
                    let val = rng.next();
                    pv = pv.push(val);
                    oracle.push(val);
                }
                (3, n) => {
                    let idx = (rng.next() as usize) % n;
                    let val = rng.next();
                    pv = pv.set(idx, val).expect("in-bounds set");
                    oracle[idx] = val;
                }
                _ => unreachable!(),
            }
            // Cheap step-end check: head + tail + a sampled interior cell.
            assert_eq!(pv.len(), oracle.len(), "len drift @ step {step}");
            if !oracle.is_empty() {
                assert_eq!(pv.get(0), oracle.first(), "head drift @ step {step}");
                assert_eq!(
                    pv.get(oracle.len() - 1),
                    oracle.last(),
                    "tail drift @ step {step}"
                );
                let probe = (rng.next() as usize) % oracle.len();
                assert_eq!(
                    pv.get(probe),
                    Some(&oracle[probe]),
                    "interior drift @ step {step}, probe {probe}"
                );
            }
        }
        // Final exhaustive sweep — every element must match.
        for i in 0..oracle.len() {
            assert_eq!(pv.get(i), Some(&oracle[i]), "final drift at {i}");
        }
        // And `iter` must traverse them in order.
        let via_iter: Vec<u64> = pv.iter().copied().collect();
        assert_eq!(via_iter, oracle, "iter drift");
    }

    /// Clone-isolation: build PV A, branch into B and C from a midpoint, mutate
    /// each independently, and verify each handle reads back its own mutations
    /// without leaking into the others.
    #[test]
    fn fuzz_oracle_clone_isolation() {
        let mut a: PersistentVec<u64> = PersistentVec::new();
        let mut oracle_a: Vec<u64> = Vec::new();
        let mut rng = Splitmix::new(0xDECAFBAD_u64);
        for _ in 0..2_000 {
            let v = rng.next();
            a = a.push(v);
            oracle_a.push(v);
        }
        // Branch.
        let mut b = a.clone();
        let mut oracle_b = oracle_a.clone();
        let mut c = a.clone();
        let mut oracle_c = oracle_a.clone();
        // Mutate B and C independently.
        for _ in 0..500 {
            let v = rng.next();
            b = b.push(v);
            oracle_b.push(v);
        }
        for _ in 0..300 {
            let idx = (rng.next() as usize) % oracle_c.len();
            let v = rng.next();
            c = c.set(idx, v).expect("in-bounds");
            oracle_c[idx] = v;
        }
        // Each handle must match its own oracle, end to end.
        for (i, &want) in oracle_a.iter().enumerate() {
            assert_eq!(a.get(i), Some(&want), "A drift at {i}");
        }
        for (i, &want) in oracle_b.iter().enumerate() {
            assert_eq!(b.get(i), Some(&want), "B drift at {i}");
        }
        for (i, &want) in oracle_c.iter().enumerate() {
            assert_eq!(c.get(i), Some(&want), "C drift at {i}");
        }
        assert_eq!(a.len(), oracle_a.len());
        assert_eq!(b.len(), oracle_b.len());
        assert_eq!(c.len(), oracle_c.len());
    }

    /// v4.39.1: `push_mut` fuzz oracle. Same shape as the `push` oracle but
    /// drives the in-place transient path so every BVT branch / tail boundary
    /// / root overflow is hit under `Arc::make_mut`. Confirms the optimization
    /// preserves the canonical `Vec<u64>` ground-truth.
    #[test]
    fn fuzz_oracle_push_mut_against_vec_u64() {
        let mut pv: PersistentVec<u64> = PersistentVec::new();
        let mut oracle: Vec<u64> = Vec::new();
        let mut rng = Splitmix::new(0xFEEDFACE_u64);
        const STEPS: usize = 100_000;
        for step in 0..STEPS {
            let val = rng.next();
            pv.push_mut(val);
            oracle.push(val);
            assert_eq!(pv.len(), oracle.len(), "len drift @ step {step}");
            if step % 1024 == 0 {
                // Cheap spot-check; full sweep at end.
                let probe = (rng.next() as usize) % oracle.len();
                assert_eq!(
                    pv.get(probe),
                    Some(&oracle[probe]),
                    "interior drift @ step {step}, probe {probe}"
                );
            }
        }
        for i in 0..oracle.len() {
            assert_eq!(pv.get(i), Some(&oracle[i]), "final drift at {i}");
        }
    }

    /// v4.39.1: critical invariant — when a `Clone`d handle B exists and the
    /// original A calls `push_mut(x)`, B's view is **not** affected (the
    /// `Arc::make_mut` tail-clone keeps the immutable contract). Without
    /// this guarantee the v4.34 BEGIN..COMMIT wrap (which holds a Catalog
    /// snapshot) would see writes leak across the snapshot boundary.
    #[test]
    fn push_mut_does_not_disturb_cloned_handle() {
        let mut a: PersistentVec<u64> = PersistentVec::new();
        for i in 0..200_u64 {
            a.push_mut(i);
        }
        let b = a.clone();
        // A pushes through the tail boundary multiple times.
        for i in 200_u64..500 {
            a.push_mut(i);
        }
        assert_eq!(b.len(), 200);
        for i in 0..200_u64 {
            assert_eq!(b.get(i as usize), Some(&i), "B drift at {i}");
        }
        assert!(b.get(200).is_none());
        assert_eq!(a.len(), 500);
        for i in 0..500_u64 {
            assert_eq!(a.get(i as usize), Some(&i), "A drift at {i}");
        }
    }

    #[test]
    fn push_clone_arc_count_stays_constant_in_old_handle() {
        // Smoke check that v4.38's O(1) clone really is Arc bumps: push 200
        // elements, take 5 clones, drop them all, verify the original is
        // unchanged. (No way to assert Arc strong_count here without exposing
        // internals — we just verify the original reads back correctly,
        // which is the property that actually matters.)
        let mut a: PersistentVec<u64> = PersistentVec::new();
        for i in 0..200_u64 {
            a = a.push(i);
        }
        let snapshots: Vec<PersistentVec<u64>> = (0..5).map(|_| a.clone()).collect();
        drop(snapshots);
        for i in 0..200_u64 {
            assert_eq!(a.get(i as usize), Some(&i));
        }
        assert_eq!(a.len(), 200);
    }
}

/// v7.39 (round 567) — see [`PersistentVec::run_cursor`].
#[derive(Debug)]
pub struct RunCursor<'a, T> {
    vec: &'a PersistentVec<T>,
    /// `(start, run)` — `run[i - start]` is element `i`.
    run: Option<(usize, &'a [T])>,
}

impl<'a, T> RunCursor<'a, T> {
    /// Element `i`, descending only when it falls outside the held run.
    pub fn get(&mut self, i: usize) -> Option<&'a T> {
        if let Some((start, run)) = self.run
            && i >= start
            && i - start < run.len()
        {
            return run.get(i - start);
        }
        let (start, run) = self.vec.run_containing(i)?;
        self.run = Some((start, run));
        run.get(i - start)
    }
}
