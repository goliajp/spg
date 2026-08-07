// v4.40 — workspace `doc-markdown` flags `B-tree`, `BTreeMap`, `Arc::clone`
// in prose contexts even when surrounding identifiers are already
// backticked; the lint is fine in source code but too noisy here.
// `many-single-char-names` flags the K / V / k / v / i naming the rest of
// the workspace already uses for map-shaped types.
#![allow(
    clippy::doc_markdown,
    clippy::many_single_char_names,
    clippy::type_complexity
)]

//! Persistent (structural-sharing) B-tree map — the v4.40 building block for
//! migrating `Table::indices` off `alloc::collections::BTreeMap`.
//!
//! `PersistentBTreeMap<K, V>` is a path-copy CoW B-tree (`ORDER = 8`,
//! `MAX_ENTRIES = 7`, `MIN_ENTRIES = 3`). Every mutating operation produces a
//! new handle that shares interior nodes with the old handle via `Arc`.
//! `Clone` is `O(1)`; `insert` and `get` are `O(log₈ N)`; a CoW path touches
//! only the spine to the affected node.
//!
//! Same hard rules as `persistent::PersistentVec`:
//! - `no_std` compatible (`alloc::sync::Arc`, `alloc::vec::Vec`).
//! - Zero `unsafe`.
//! - Zero external deps.
//!
//! Layout (traditional B-tree, *not* B+ tree — entries live at every level,
//! including internal nodes; descending hits a value if and only if the key
//! sits along the spine):
//!
//!   enum BNode<K, V> {
//!       Leaf { entries: Vec<(K, V)> },                    // entries.len() ∈ [1, MAX_ENTRIES]
//!       Internal {
//!           entries: Vec<(K, V)>,                          // entries.len() ∈ [1, MAX_ENTRIES]
//!           children: Vec<Arc<BNode<K, V>>>,              // children.len() == entries.len() + 1
//!       },
//!   }
//!
//! Invariants (debug-checked in `#[cfg(test)]` only):
//! - Every internal node satisfies `children.len() == entries.len() + 1`.
//! - Entries inside any single node are sorted strictly ascending by `K`.
//! - The root may have fewer than `MIN_ENTRIES`; every other node has ≥
//!   `MIN_ENTRIES`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Bound;

/// B-tree order (max children per internal node). Picked at the small end of
/// the conventional 8–16 range to keep per-CoW node-clone cost low — the
/// path-copy hits one node per level, and each cloned node carries up to
/// `MAX_ENTRIES` of `(K, V)`.
const ORDER: usize = 8;
const MAX_ENTRIES: usize = ORDER - 1; // 7
const MAX_CHILDREN: usize = ORDER; // 8

#[derive(Debug)]
enum BNode<K, V> {
    Leaf {
        entries: Vec<(K, V)>,
    },
    Internal {
        entries: Vec<(K, V)>,
        children: Vec<Arc<BNode<K, V>>>,
    },
}

// Manual `Clone` impl so the bound only applies when `Arc::make_mut`
// (the v4.40.1 transient path) actually needs it. The non-mutating
// `get` / `iter` paths stay generic over any `K`, `V`.
impl<K: Clone, V: Clone> Clone for BNode<K, V> {
    fn clone(&self) -> Self {
        match self {
            Self::Leaf { entries } => Self::Leaf {
                entries: entries.clone(),
            },
            Self::Internal { entries, children } => Self::Internal {
                entries: entries.clone(),
                children: children.clone(),
            },
        }
    }
}

/// A persistent ordered map. `Clone` is `O(1)`; `insert` returns a new handle
/// that shares unaffected subtrees with the old via `Arc::clone`.
#[derive(Debug)]
pub struct PersistentBTreeMap<K, V> {
    root: Arc<BNode<K, V>>,
    len: usize,
}

impl<K, V> Default for PersistentBTreeMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Clone for PersistentBTreeMap<K, V> {
    /// O(1) — `Arc` bump on the root. The whole reason this type exists in
    /// v4.40 is to make `Table::indices: Vec<Index>` cheap to clone once
    /// the inner `BTreeMap` is replaced.
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            len: self.len,
        }
    }
}

impl<K: PartialEq, V: PartialEq> PartialEq for PersistentBTreeMap<K, V>
where
    K: Ord,
{
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<K: Eq + Ord, V: Eq> Eq for PersistentBTreeMap<K, V> {}

impl<K, V> PersistentBTreeMap<K, V> {
    /// Empty map. Builds one empty `Leaf` root; subsequent inserts grow
    /// the trie outward when overflowing `MAX_ENTRIES`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(BNode::Leaf {
                entries: Vec::new(),
            }),
            len: 0,
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
}

/// v7.37.43 (INSUBQ B-3) — internal-node search outcome: either the
/// key matched an entry directly, or the search bisected and the
/// caller must descend into `children[i]`.
enum FoundOrDescend {
    Found(usize),
    Descend(usize),
}

/// v7.37.43 (INSUBQ B-3) — linear search a leaf's entries for `key`.
/// `entries.len() ≤ MAX_ENTRIES = 7`; linear-scan beats binary_search
/// at this size on modern branch predictors. Returns the entry index
/// when present.
#[inline]
fn linear_find_entry<K: Ord, V>(entries: &[(K, V)], key: &K) -> Option<usize> {
    for (i, (k, _)) in entries.iter().enumerate() {
        match k.cmp(key) {
            core::cmp::Ordering::Equal => return Some(i),
            core::cmp::Ordering::Greater => return None,
            core::cmp::Ordering::Less => continue,
        }
    }
    None
}

/// v7.37.43 (INSUBQ B-3) — linear search an internal node's entries
/// for `key`. Returns Found(idx) if key matches an entry, otherwise
/// Descend(idx) with the child slot to follow.
#[inline]
fn linear_position_internal<K: Ord, V>(entries: &[(K, V)], key: &K) -> FoundOrDescend {
    for (i, (k, _)) in entries.iter().enumerate() {
        match k.cmp(key) {
            core::cmp::Ordering::Equal => return FoundOrDescend::Found(i),
            core::cmp::Ordering::Greater => return FoundOrDescend::Descend(i),
            core::cmp::Ordering::Less => continue,
        }
    }
    FoundOrDescend::Descend(entries.len())
}

impl<K: Ord, V> PersistentBTreeMap<K, V> {
    /// `O(log₈ N)`. Per-node search at each level; on hit returns the
    /// value, on miss descends into the child between adjacent entries.
    ///
    /// v7.37.43 (INSUBQ B-3) — every node holds ≤ `MAX_ENTRIES = 7`
    /// `(K, V)` pairs, so the inner search is over at most 7 elements.
    /// `binary_search_by` issues a data-dependent branch per probe;
    /// for n ≤ 7 a straight linear scan with a single ordering compare
    /// per element wins on modern branch predictors and has tighter
    /// codegen (no early-exit on found-vs-bisect handling). Probing a
    /// 100k-entry index on 1k keys (the INSUBQ shape) cuts ~30-60 ns
    /// per descent × ~5 levels × 1k keys ≈ 150-300 µs off the loop.
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut node: &Arc<BNode<K, V>> = &self.root;
        loop {
            match &**node {
                BNode::Leaf { entries } => {
                    return linear_find_entry(entries, key).map(|i| &entries[i].1);
                }
                BNode::Internal { entries, children } => {
                    match linear_position_internal(entries, key) {
                        FoundOrDescend::Found(i) => return Some(&entries[i].1),
                        FoundOrDescend::Descend(i) => {
                            node = &children[i];
                        }
                    }
                }
            }
        }
    }

    /// v7.39 (round 215) — the entry with the largest key STRICTLY less than
    /// `key` (the in-order predecessor of `key`), or `None` when every key is
    /// ≥ `key`. `O(log₈ N)`: descend once toward `key`; at each internal node
    /// the entries left of the descent slot are all < `key`, and the rightmost
    /// of them is the best candidate at that level — but the child we descend
    /// into holds keys strictly between it and `key`, so a deeper hit always
    /// overrides. The building block for the range-exclusion overlap probe
    /// (find the existing range whose lower bound sits just below a candidate).
    pub fn predecessor(&self, key: &K) -> Option<(&K, &V)> {
        let mut node: &Arc<BNode<K, V>> = &self.root;
        let mut best: Option<(&K, &V)> = None;
        loop {
            match &**node {
                BNode::Leaf { entries } => {
                    let i = entries.partition_point(|e| &e.0 < key);
                    if i > 0 {
                        let (k, v) = &entries[i - 1];
                        best = Some((k, v));
                    }
                    return best;
                }
                BNode::Internal { entries, children } => {
                    let i = entries.partition_point(|e| &e.0 < key);
                    if i > 0 {
                        let (k, v) = &entries[i - 1];
                        best = Some((k, v));
                    }
                    node = &children[i];
                }
            }
        }
    }

    /// In-order key-then-value iterator. Used by `PartialEq` and any caller
    /// that needs to walk the whole map (e.g. catalog deserialization).
    /// v7.39 (round 170) — bulk-build from PRE-SORTED entries, bottom-up.
    /// The per-row `insert_mut` path pays a path-copy allocation per
    /// element (~300ns each), which made every `rebuild_indices` pass
    /// O(n) allocations per index (the dominant cost of a VACUUM on an
    /// indexed table). This builds leaves in ~ORDER-sized runs and
    /// levels them up with the run separators as the internal entries —
    /// zero path copies. Groups are cut evenly so no node is left
    /// pathologically underfull for the later insert/remove rebalance.
    ///
    /// Debug builds assert the input is strictly sorted by `K`.
    #[must_use]
    pub fn from_sorted(entries: Vec<(K, V)>) -> Self
    where
        K: Ord + Clone,
        V: Clone,
    {
        #[cfg(debug_assertions)]
        for w in entries.windows(2) {
            debug_assert!(w[0].0 < w[1].0, "from_sorted requires strictly sorted keys");
        }
        let len = entries.len();
        if len <= MAX_ENTRIES {
            return Self {
                root: Arc::new(BNode::Leaf { entries }),
                len,
            };
        }
        // Level 0 — cut into leaf runs with one separator entry between
        // consecutive leaves. Choose the number of leaves so every leaf
        // gets between ceil(MAX_ENTRIES/2) and MAX_ENTRIES entries.
        let mut nodes: Vec<Arc<BNode<K, V>>> = Vec::new();
        let mut seps: Vec<(K, V)> = Vec::new();
        {
            // n items into g groups of ≤ MAX_ENTRIES with g-1 separators:
            // g = ceil((n + 1) / (MAX_ENTRIES + 1)).
            let g = (len + 1).div_ceil(MAX_ENTRIES + 1);
            let mut it = entries.into_iter();
            let mut remaining = len;
            for gi in 0..g {
                let groups_left = g - gi;
                // Evenly split what's left (minus the separators still owed).
                let seps_left = groups_left - 1;
                let take = (remaining - seps_left).div_ceil(groups_left);
                let leaf: Vec<(K, V)> = (&mut it).take(take).collect();
                remaining -= leaf.len();
                nodes.push(Arc::new(BNode::Leaf { entries: leaf }));
                if gi + 1 < g {
                    let sep = it.next().expect("separator exists");
                    remaining -= 1;
                    seps.push(sep);
                }
            }
        }
        // Level up until one root remains: group ≤ MAX_CHILDREN children
        // with the intra-group separators as the internal entries; the
        // inter-group separators bubble to the next level.
        while nodes.len() > 1 {
            let g = nodes.len().div_ceil(MAX_CHILDREN);
            let per = nodes.len().div_ceil(g);
            let mut up_nodes: Vec<Arc<BNode<K, V>>> = Vec::with_capacity(g);
            let mut up_seps: Vec<(K, V)> = Vec::with_capacity(g - 1);
            let mut node_it = nodes.into_iter();
            let mut sep_it = seps.into_iter();
            let mut children: Vec<Arc<BNode<K, V>>> = Vec::with_capacity(per);
            let mut inner: Vec<(K, V)> = Vec::with_capacity(per - 1);
            loop {
                match node_it.next() {
                    Some(n) => {
                        if !children.is_empty() {
                            // The separator BEFORE this child: intra-group
                            // if the group isn't full, else it bubbles up.
                            let sep = sep_it.next().expect("separator per boundary");
                            if children.len() < per {
                                inner.push(sep);
                            } else {
                                up_nodes.push(Arc::new(BNode::Internal {
                                    entries: core::mem::take(&mut inner),
                                    children: core::mem::take(&mut children),
                                }));
                                up_seps.push(sep);
                            }
                        }
                        children.push(n);
                    }
                    None => {
                        up_nodes.push(Arc::new(BNode::Internal {
                            entries: inner,
                            children,
                        }));
                        break;
                    }
                }
            }
            nodes = up_nodes;
            seps = up_seps;
        }
        Self {
            root: nodes.pop().expect("one root"),
            len,
        }
    }

    pub fn iter(&self) -> Iter<'_, K, V> {
        let mut stack: Vec<(&Arc<BNode<K, V>>, usize)> = Vec::with_capacity(8);
        stack.push((&self.root, 0));
        Iter { stack }
    }

    /// v7.34.4 — descending-order iterator. Mirrors `iter()` but the
    /// per-node walk visits child-then-entry pairs right-to-left.
    /// Used by the ORDER BY `<indexed col>` DESC + LIMIT N executor
    /// path to walk only the first N matches off the rightmost leaf
    /// instead of materialising every row + partial-sorting.
    pub fn iter_rev(&self) -> IterRev<'_, K, V> {
        let mut stack: Vec<(&Arc<BNode<K, V>>, usize)> = Vec::with_capacity(8);
        stack.push((&self.root, 1));
        IterRev { stack }
    }

    /// v7.38 (perf, index range scan) — in-order iterator over the entries
    /// whose keys fall in `(lo, hi)` (each end honoured per `core::ops::Bound`).
    /// Descends to `lo` in `O(log₈ N)` (skipping the subtrees entirely below
    /// it) by building the same `(node, child_index)` cursor stack `iter()`
    /// uses, positioned at the first key ≥/> `lo`; then walks forward and stops
    /// at the first key past `hi`. `O(log N + k)` for `k` hits — the building
    /// block for `Index::lookup_range` (BETWEEN / `>` / `<` seeks).
    pub fn range<'a>(&'a self, lo: Bound<&K>, hi: Bound<&K>) -> RangeIter<'a, K, V>
    where
        K: Clone,
    {
        let mut stack: Vec<(&'a Arc<BNode<K, V>>, usize)> = Vec::with_capacity(8);
        let mut node = &self.root;
        loop {
            match &**node {
                BNode::Leaf { entries } => {
                    // Leaf frame: begin emitting at the first in-range entry.
                    stack.push((node, lower_index(entries, lo)));
                    break;
                }
                BNode::Internal { entries, children } => {
                    let i = lower_index(entries, lo);
                    // children[i] may hold keys ≥ lo and < entries[i]; descend
                    // into it, and set this frame to resume by emitting
                    // entries[i] once that subtree is exhausted (phase-1 slot i
                    // → idx = 2*i + 1, matching `Iter::next`'s frame encoding).
                    stack.push((node, 2 * i + 1));
                    node = &children[i];
                }
            }
        }
        let (hi_key, hi_incl) = match hi {
            Bound::Unbounded => (None, false),
            Bound::Included(k) => (Some(k.clone()), true),
            Bound::Excluded(k) => (Some(k.clone()), false),
        };
        RangeIter {
            inner: Iter { stack },
            hi_key,
            hi_incl,
            done: false,
        }
    }
}

/// First entry index whose key is ≥ (Included) / > (Excluded) `lo`; 0 for
/// Unbounded. Linear — a node holds ≤ `MAX_ENTRIES` = 7 entries.
fn lower_index<K: Ord, V>(entries: &[(K, V)], lo: Bound<&K>) -> usize {
    match lo {
        Bound::Unbounded => 0,
        Bound::Included(k) => entries.partition_point(|e| &e.0 < k),
        Bound::Excluded(k) => entries.partition_point(|e| &e.0 <= k),
    }
}

impl<K: Ord + Clone, V: Clone> PersistentBTreeMap<K, V> {
    /// `O(log₈ N)`. Path-copy insert; replaces if `key` exists, otherwise
    /// inserts and grows by 1. Returns `(new_map, previous_value)`.
    #[must_use]
    pub fn insert(&self, key: K, value: V) -> (Self, Option<V>) {
        let (new_left, split, prev_v) = insert_helper(&self.root, key, value);
        let new_root = if let Some((right, median)) = split {
            Arc::new(BNode::Internal {
                entries: alloc::vec![median],
                children: alloc::vec![new_left, right],
            })
        } else {
            new_left
        };
        let new_len = if prev_v.is_none() {
            self.len + 1
        } else {
            self.len
        };
        (
            Self {
                root: new_root,
                len: new_len,
            },
            prev_v,
        )
    }

    /// `O(log₈ N)` transient insert. v4.40.1 perf path: walks
    /// `Arc::make_mut` down the spine — when the spine `Arc`s are uniquely
    /// owned (the common case in `Table::insert` outside a TX wrap), every
    /// touched node mutates in place at roughly `std::BTreeMap::insert`
    /// cost. When a cloned handle is outstanding (e.g. a Catalog snapshot
    /// inside a TX wrap), `Arc::make_mut` path-copies just the affected
    /// node and the snapshot stays untouched. Either way, callers see the
    /// same end state as the immutable `insert` followed by reassignment.
    pub fn insert_mut(&mut self, key: K, value: V) -> Option<V> {
        let (split, prev_v) = insert_transient_helper(&mut self.root, key, value);
        if let Some((right, median)) = split {
            // Root overflow: wrap the old root + new right sibling under a
            // fresh top-level Internal carrying the median entry. We need
            // to take ownership of self.root to move it into `children`,
            // so swap in a placeholder Leaf and then overwrite with the
            // real new root below.
            let old_root = core::mem::replace(
                &mut self.root,
                Arc::new(BNode::Leaf {
                    entries: Vec::new(),
                }),
            );
            self.root = Arc::new(BNode::Internal {
                entries: alloc::vec![median],
                children: alloc::vec![old_root, right],
            });
        }
        if prev_v.is_none() {
            self.len += 1;
        }
        prev_v
    }

    /// `O(log₈ N)` transient remove. Returns the value that was stored
    /// under `key`, or `None` when the key was absent (the map is then
    /// untouched).
    ///
    /// v7.39 (round 465) — the map had no removal at all: `new / get /
    /// predecessor / from_sorted / iter / iter_rev / range / insert /
    /// insert_mut`. That is why dropping a single index entry meant
    /// rebuilding the whole map from the rows, and why one autovacuum tick
    /// costs 11 ms on a 50k-row table with one secondary index — five
    /// times the INSERT it exists to protect, all of it under the engine
    /// write lock. Round 464 measured a filtered rebuild and it lost to
    /// the existing from-the-rows rebuild, because iterating a
    /// structurally-shared tree is pointer chasing while the rebuild is a
    /// linear scan plus one sort. Removal is the operation that was
    /// missing; with it, reclaiming k rows touches k spines instead of
    /// rebuilding n entries.
    ///
    /// Walks `Arc::make_mut` down the spine like `insert_mut`, so a
    /// uniquely-owned tree mutates in place and an outstanding snapshot
    /// (a Catalog clone inside a TX wrap) path-copies only the spine.
    pub fn remove_mut(&mut self, key: &K) -> Option<V> {
        let removed = remove_transient_helper(&mut self.root, key)?;
        // The root is the one node allowed to underflow, but an internal
        // root emptied of entries has exactly one child and must be
        // replaced by it or the tree grows a permanently useless level.
        let collapse = match self.root.as_ref() {
            BNode::Internal { entries, children } if entries.is_empty() => {
                debug_assert_eq!(children.len(), 1);
                children.first().cloned()
            }
            _ => None,
        };
        if let Some(only) = collapse {
            self.root = only;
        }
        self.len -= 1;
        Some(removed)
    }

    /// Immutable removal, for symmetry with [`Self::insert`]. Returns
    /// `(new_map, previous_value)`; the receiver is untouched.
    #[must_use]
    pub fn remove(&self, key: &K) -> (Self, Option<V>) {
        let mut next = self.clone();
        let prev = next.remove_mut(key);
        (next, prev)
    }
}

/// Minimum entries in any node but the root. A node that drops below this
/// borrows from a sibling, or merges with one.
const MIN_ENTRIES: usize = ORDER / 2 - 1; // 3

impl<K, V> BNode<K, V> {
    fn entry_count(&self) -> usize {
        match self {
            BNode::Leaf { entries } | BNode::Internal { entries, .. } => entries.len(),
        }
    }
}

/// Transient remove worker. Returns the removed value, or `None` when the
/// key is not in this subtree (in which case nothing was modified).
fn remove_transient_helper<K: Ord + Clone, V: Clone>(
    node: &mut Arc<BNode<K, V>>,
    key: &K,
) -> Option<V> {
    // Probe before `make_mut`: a miss must not path-copy the spine.
    let (found, idx) = match node.as_ref() {
        BNode::Leaf { entries } | BNode::Internal { entries, .. } => {
            match entries.binary_search_by(|(ek, _)| ek.cmp(key)) {
                Ok(i) => (true, i),
                Err(i) => (false, i),
            }
        }
    };
    if !found && matches!(node.as_ref(), BNode::Leaf { .. }) {
        return None;
    }
    let inner = Arc::make_mut(node);
    match inner {
        BNode::Leaf { entries } => Some(entries.remove(idx).1),
        BNode::Internal { entries, children } => {
            if found {
                // Standard B-tree interior delete: swap in the in-order
                // predecessor, which always lives in a leaf, then repair
                // the subtree it came out of.
                let pred = remove_max(&mut children[idx]);
                let old = core::mem::replace(&mut entries[idx], pred);
                fix_child(entries, children, idx);
                Some(old.1)
            } else {
                let removed = remove_transient_helper(&mut children[idx], key)?;
                fix_child(entries, children, idx);
                Some(removed)
            }
        }
    }
}

/// Detach the largest entry of this subtree. The subtree must be non-empty;
/// callers only reach it from an internal node whose children are populated.
fn remove_max<K: Ord + Clone, V: Clone>(node: &mut Arc<BNode<K, V>>) -> (K, V) {
    let inner = Arc::make_mut(node);
    match inner {
        BNode::Leaf { entries } => entries.pop().expect("a B-tree leaf is never empty"),
        BNode::Internal { entries, children } => {
            let last = children.len() - 1;
            let kv = remove_max(&mut children[last]);
            fix_child(entries, children, last);
            kv
        }
    }
}

/// Restore `children[i]`'s minimum occupancy by borrowing from a sibling, or
/// merging with one when neither sibling can spare an entry.
fn fix_child<K: Ord + Clone, V: Clone>(
    entries: &mut Vec<(K, V)>,
    children: &mut Vec<Arc<BNode<K, V>>>,
    i: usize,
) {
    if children[i].entry_count() >= MIN_ENTRIES {
        return;
    }
    if i > 0 && children[i - 1].entry_count() > MIN_ENTRIES {
        rotate_from_left(entries, children, i);
    } else if i + 1 < children.len() && children[i + 1].entry_count() > MIN_ENTRIES {
        rotate_from_right(entries, children, i);
    } else if i > 0 {
        merge_children(entries, children, i - 1);
    } else {
        merge_children(entries, children, i);
    }
}

/// Move the left sibling's largest entry up into the separator slot and the
/// old separator down into `children[i]`'s front.
fn rotate_from_left<K: Ord + Clone, V: Clone>(
    entries: &mut [(K, V)],
    children: &mut [Arc<BNode<K, V>>],
    i: usize,
) {
    let (moved_entry, moved_child) = match Arc::make_mut(&mut children[i - 1]) {
        BNode::Leaf { entries: le } => (le.pop().expect("sibling has entries to spare"), None),
        BNode::Internal {
            entries: le,
            children: lc,
        } => (
            le.pop().expect("sibling has entries to spare"),
            Some(
                lc.pop()
                    .expect("internal node has entries.len()+1 children"),
            ),
        ),
    };
    let separator = core::mem::replace(&mut entries[i - 1], moved_entry);
    match Arc::make_mut(&mut children[i]) {
        BNode::Leaf { entries: ce } => {
            debug_assert!(moved_child.is_none());
            ce.insert(0, separator);
        }
        BNode::Internal {
            entries: ce,
            children: cc,
        } => {
            ce.insert(0, separator);
            cc.insert(
                0,
                moved_child.expect("sibling of an internal node is internal"),
            );
        }
    }
}

/// Mirror of [`rotate_from_left`] using the right sibling.
fn rotate_from_right<K: Ord + Clone, V: Clone>(
    entries: &mut [(K, V)],
    children: &mut [Arc<BNode<K, V>>],
    i: usize,
) {
    let (moved_entry, moved_child) = match Arc::make_mut(&mut children[i + 1]) {
        BNode::Leaf { entries: re } => (re.remove(0), None),
        BNode::Internal {
            entries: re,
            children: rc,
        } => (re.remove(0), Some(rc.remove(0))),
    };
    let separator = core::mem::replace(&mut entries[i], moved_entry);
    match Arc::make_mut(&mut children[i]) {
        BNode::Leaf { entries: ce } => {
            debug_assert!(moved_child.is_none());
            ce.push(separator);
        }
        BNode::Internal {
            entries: ce,
            children: cc,
        } => {
            ce.push(separator);
            cc.push(moved_child.expect("sibling of an internal node is internal"));
        }
    }
}

/// Fold `children[sep + 1]` and the separator entry into `children[sep]`.
/// Both children are at minimum occupancy, so the result fits.
fn merge_children<K: Ord + Clone, V: Clone>(
    entries: &mut Vec<(K, V)>,
    children: &mut Vec<Arc<BNode<K, V>>>,
    sep: usize,
) {
    let separator = entries.remove(sep);
    let right = children.remove(sep + 1);
    let right = Arc::try_unwrap(right).unwrap_or_else(|shared| (*shared).clone());
    match (Arc::make_mut(&mut children[sep]), right) {
        (BNode::Leaf { entries: le }, BNode::Leaf { entries: re }) => {
            le.push(separator);
            le.extend(re);
        }
        (
            BNode::Internal {
                entries: le,
                children: lc,
            },
            BNode::Internal {
                entries: re,
                children: rc,
            },
        ) => {
            le.push(separator);
            le.extend(re);
            lc.extend(rc);
        }
        // Siblings are always at the same depth, so the mixed cases are
        // unreachable; putting the separator back keeps the map a valid
        // (if larger) tree rather than losing an entry.
        (BNode::Leaf { entries: le }, BNode::Internal { .. })
        | (BNode::Internal { entries: le, .. }, BNode::Leaf { .. }) => {
            debug_assert!(false, "B-tree siblings must be at the same depth");
            le.push(separator);
        }
    }
}

/// Transient insert worker — walks `Arc::make_mut` down the spine so each
/// uniquely-owned node mutates in place. Splits still allocate fresh
/// `Arc<BNode>` for the new right sibling (those are genuinely new nodes,
/// not CoW copies).
fn insert_transient_helper<K: Ord + Clone, V: Clone>(
    node: &mut Arc<BNode<K, V>>,
    k: K,
    v: V,
) -> (Option<(Arc<BNode<K, V>>, (K, V))>, Option<V>) {
    let inner = Arc::make_mut(node);
    match inner {
        BNode::Leaf { entries } => {
            let pos = entries.binary_search_by(|(ek, _)| ek.cmp(&k));
            let prev_v = match pos {
                Ok(idx) => Some(core::mem::replace(&mut entries[idx].1, v)),
                Err(idx) => {
                    entries.insert(idx, (k, v));
                    None
                }
            };
            if entries.len() <= MAX_ENTRIES {
                return (None, prev_v);
            }
            // Overflow: split (same arithmetic as the immutable path).
            let mid = entries.len() / 2;
            let right_entries = entries.split_off(mid + 1);
            let median = entries.pop().expect("mid was in-bounds");
            let right = Arc::new(BNode::Leaf {
                entries: right_entries,
            });
            (Some((right, median)), prev_v)
        }
        BNode::Internal { entries, children } => {
            let pos = entries.binary_search_by(|(ek, _)| ek.cmp(&k));
            match pos {
                Ok(idx) => {
                    let prev_v = core::mem::replace(&mut entries[idx].1, v);
                    (None, Some(prev_v))
                }
                Err(idx) => {
                    let (split, prev_v) = insert_transient_helper(&mut children[idx], k, v);
                    if let Some((right_sibling, median)) = split {
                        entries.insert(idx, median);
                        children.insert(idx + 1, right_sibling);
                    }
                    if children.len() <= MAX_CHILDREN {
                        return (None, prev_v);
                    }
                    let mid = entries.len() / 2;
                    let right_entries = entries.split_off(mid + 1);
                    let median = entries.pop().expect("mid was in-bounds");
                    let right_children = children.split_off(mid + 1);
                    let right = Arc::new(BNode::Internal {
                        entries: right_entries,
                        children: right_children,
                    });
                    (Some((right, median)), prev_v)
                }
            }
        }
    }
}

/// Recursive insert worker. Returns `(new_left_node, optional_split, prev_v)`
/// where `optional_split = Some((right_sibling, median_entry))` when this
/// node overflowed; the caller bubbles the split up.
fn insert_helper<K: Ord + Clone, V: Clone>(
    node: &Arc<BNode<K, V>>,
    k: K,
    v: V,
) -> (
    Arc<BNode<K, V>>,
    Option<(Arc<BNode<K, V>>, (K, V))>,
    Option<V>,
) {
    match &**node {
        BNode::Leaf { entries } => {
            let pos = entries.binary_search_by(|(ek, _)| ek.cmp(&k));
            let mut new_entries = entries.clone();
            let prev_v = match pos {
                Ok(idx) => Some(core::mem::replace(&mut new_entries[idx].1, v)),
                Err(idx) => {
                    new_entries.insert(idx, (k, v));
                    None
                }
            };
            if new_entries.len() <= MAX_ENTRIES {
                return (
                    Arc::new(BNode::Leaf {
                        entries: new_entries,
                    }),
                    None,
                    prev_v,
                );
            }
            // Overflow: split. With MAX_ENTRIES = 7 the overflowed leaf holds
            // 8 entries → 4 left + median + 3 right (or vice versa). Each
            // half retains ≥ MIN_ENTRIES = 3.
            let mid = new_entries.len() / 2; // 4
            let right_entries = new_entries.split_off(mid + 1);
            let median = new_entries.pop().expect("mid was in-bounds");
            let left = Arc::new(BNode::Leaf {
                entries: new_entries,
            });
            let right = Arc::new(BNode::Leaf {
                entries: right_entries,
            });
            (left, Some((right, median)), prev_v)
        }
        BNode::Internal { entries, children } => {
            let pos = entries.binary_search_by(|(ek, _)| ek.cmp(&k));
            match pos {
                Ok(idx) => {
                    // Key already lives on this internal node — replace V.
                    let mut new_entries = entries.clone();
                    let prev_v = core::mem::replace(&mut new_entries[idx].1, v);
                    (
                        Arc::new(BNode::Internal {
                            entries: new_entries,
                            children: children.clone(),
                        }),
                        None,
                        Some(prev_v),
                    )
                }
                Err(idx) => {
                    // Descend into children[idx]; bubble up any split.
                    let (new_child, split, prev_v) = insert_helper(&children[idx], k, v);
                    let mut new_entries = entries.clone();
                    let mut new_children = children.clone();
                    new_children[idx] = new_child;
                    if let Some((right_sibling, median)) = split {
                        new_entries.insert(idx, median);
                        new_children.insert(idx + 1, right_sibling);
                    }
                    if new_children.len() <= MAX_CHILDREN {
                        return (
                            Arc::new(BNode::Internal {
                                entries: new_entries,
                                children: new_children,
                            }),
                            None,
                            prev_v,
                        );
                    }
                    // Internal overflow: 8 entries + 9 children → split into
                    // 4-entry + 5-child left, median goes up, 3-entry +
                    // 4-child right. Both halves keep ≥ MIN_ENTRIES = 3 and
                    // ≥ MIN_CHILDREN = 4.
                    let mid = new_entries.len() / 2; // 4
                    let right_entries = new_entries.split_off(mid + 1);
                    let median = new_entries.pop().expect("mid was in-bounds");
                    let right_children = new_children.split_off(mid + 1);
                    let left = Arc::new(BNode::Internal {
                        entries: new_entries,
                        children: new_children,
                    });
                    let right = Arc::new(BNode::Internal {
                        entries: right_entries,
                        children: right_children,
                    });
                    (left, Some((right, median)), prev_v)
                }
            }
        }
    }
}

/// In-order `(K, V)` iterator. Uses an explicit stack with `(node, child_index)`
/// frames so we don't need to copy entries into a flat buffer.
#[derive(Debug)]
pub struct Iter<'a, K, V> {
    stack: Vec<(&'a Arc<BNode<K, V>>, usize)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        loop {
            let (node, idx) = *self.stack.last()?;
            match &**node {
                BNode::Leaf { entries } => {
                    if idx < entries.len() {
                        let (k, v) = &entries[idx];
                        self.stack.last_mut().unwrap().1 = idx + 1;
                        return Some((k, v));
                    }
                    self.stack.pop();
                }
                BNode::Internal { entries, children } => {
                    // Frame layout: child_index `idx` means "we still need
                    // to descend into children[idx / 2]" (even) or "emit
                    // entries[idx / 2]" (odd). Encoding two phases per
                    // entry slot.
                    let phase = idx & 1;
                    let slot = idx >> 1;
                    if phase == 0 {
                        // Descend into children[slot] if it exists.
                        if slot < children.len() {
                            self.stack.last_mut().unwrap().1 = idx + 1;
                            self.stack.push((&children[slot], 0));
                            continue;
                        }
                        self.stack.pop();
                    } else {
                        // Emit entries[slot] if it exists.
                        if slot < entries.len() {
                            let (k, v) = &entries[slot];
                            self.stack.last_mut().unwrap().1 = idx + 1;
                            return Some((k, v));
                        }
                        self.stack.pop();
                    }
                }
            }
        }
    }
}

/// v7.38 — bounded-above wrapper over [`Iter`], produced by
/// [`PersistentBTreeMap::range`]. `Iter` already starts at `lo` (the seek
/// stack); this stops emission at the first key past `hi`.
#[derive(Debug)]
pub struct RangeIter<'a, K, V> {
    inner: Iter<'a, K, V>,
    hi_key: Option<K>,
    hi_incl: bool,
    done: bool,
}

impl<'a, K: Ord, V> Iterator for RangeIter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        if self.done {
            return None;
        }
        let (k, v) = self.inner.next()?;
        if let Some(h) = &self.hi_key {
            let past = if self.hi_incl { k > h } else { k >= h };
            if past {
                self.done = true;
                return None;
            }
        }
        Some((k, v))
    }
}

/// v7.34.4 — descending-order `(K, V)` iterator. Mirrors `Iter` but each
/// node's child-then-entry walk runs right-to-left so the first yielded
/// pair is the maximum key in the map. Used by the ORDER BY `<indexed
/// col>` DESC + LIMIT N executor path to walk only the first N matches
/// off the rightmost leaf instead of materialising every row + partial-
/// sorting; the existing forward `Iter` stays untouched so unrelated
/// callers (catalog deserialisation, PartialEq) are unaffected.
#[derive(Debug)]
pub struct IterRev<'a, K, V> {
    // (node, next_pos) where next_pos counts the remaining reverse
    // step within the node, starting at 1. A pos > step_count means
    // the node is exhausted (pop). For a Leaf with E entries the
    // step count is E (emit entries right-to-left). For an Internal
    // node with E entries / E+1 children the step count is 2E+1: odd
    // positions descend into a child, even positions emit an entry,
    // both walking right-to-left.
    stack: Vec<(&'a Arc<BNode<K, V>>, usize)>,
}

impl<'a, K, V> Iterator for IterRev<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        loop {
            let (node, pos) = *self.stack.last()?;
            match &**node {
                BNode::Leaf { entries } => {
                    if pos <= entries.len() {
                        let i = entries.len() - pos;
                        self.stack.last_mut().unwrap().1 = pos + 1;
                        let (k, v) = &entries[i];
                        return Some((k, v));
                    }
                    self.stack.pop();
                }
                BNode::Internal { entries, children } => {
                    let n_steps = 2 * entries.len() + 1;
                    if pos <= n_steps {
                        self.stack.last_mut().unwrap().1 = pos + 1;
                        if pos % 2 == 1 {
                            // Odd: descend into `children[E - (pos-1)/2]`.
                            let child_idx = entries.len() - (pos - 1) / 2;
                            self.stack.push((&children[child_idx], 1));
                            continue;
                        }
                        // Even: emit `entries[E - pos/2]`.
                        let entry_idx = entries.len() - pos / 2;
                        let (k, v) = &entries[entry_idx];
                        return Some((k, v));
                    }
                    self.stack.pop();
                }
            }
        }
    }
}

impl<'a, K: Ord, V> IntoIterator for &'a PersistentBTreeMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
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
    use alloc::collections::BTreeMap;
    use alloc::vec;

    // ---- round 465: structural checks for removal ----

    /// Depth of every leaf, or `None` when they disagree. Also asserts each
    /// node's local shape: entry count in range, entries strictly ascending,
    /// and `children.len() == entries.len() + 1`.
    fn check_node<K: Ord + core::fmt::Debug, V>(
        node: &BNode<K, V>,
        is_root: bool,
        depth: usize,
    ) -> usize {
        let entries = match node {
            BNode::Leaf { entries } | BNode::Internal { entries, .. } => entries,
        };
        assert!(
            entries.len() <= MAX_ENTRIES,
            "node overfull: {} entries",
            entries.len()
        );
        if !is_root {
            assert!(
                entries.len() >= MIN_ENTRIES,
                "non-root node underfull: {} entries",
                entries.len()
            );
        }
        for w in entries.windows(2) {
            assert!(w[0].0 < w[1].0, "entries out of order inside a node");
        }
        match node {
            BNode::Leaf { .. } => depth,
            BNode::Internal { entries, children } => {
                assert_eq!(
                    children.len(),
                    entries.len() + 1,
                    "internal node must have entries.len()+1 children"
                );
                let mut seen: Option<usize> = None;
                for c in children {
                    let d = check_node(c, false, depth + 1);
                    match seen {
                        None => seen = Some(d),
                        Some(prev) => assert_eq!(prev, d, "leaves at different depths"),
                    }
                }
                seen.expect("internal node has children")
            }
        }
    }

    fn check_map<K: Ord + Clone + core::fmt::Debug, V: Clone>(m: &PersistentBTreeMap<K, V>) {
        check_node(m.root.as_ref(), true, 0);
        // In-order traversal must be globally sorted, and `len` must match.
        let keys: Vec<&K> = m.iter().map(|(k, _)| k).collect();
        for w in keys.windows(2) {
            assert!(w[0] < w[1], "iteration order is not globally sorted");
        }
        assert_eq!(keys.len(), m.len(), "len disagrees with iteration");
    }

    /// Deterministic sequence — no rand dependency, and a failure is
    /// reproducible from the seed alone.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *state >> 33
    }

    #[test]
    fn round465_remove_absent_key_is_a_no_op() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..50_i64 {
            pb.insert_mut(i * 2, i);
        }
        let before = pb.len();
        assert_eq!(pb.remove_mut(&7), None);
        assert_eq!(pb.remove_mut(&-1), None);
        assert_eq!(pb.remove_mut(&1000), None);
        assert_eq!(pb.len(), before);
        check_map(&pb);
    }

    #[test]
    fn round465_remove_every_key_empties_the_map() {
        for n in [1_i64, 7, 8, 9, 100, 500] {
            let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
            for i in 0..n {
                pb.insert_mut(i, i * 3);
            }
            for i in 0..n {
                assert_eq!(pb.remove_mut(&i), Some(i * 3), "n={n} i={i}");
                assert_eq!(pb.len() as i64, n - i - 1);
                check_map(&pb);
                // Everything not yet removed must still be reachable.
                for j in (i + 1)..n {
                    assert_eq!(
                        pb.get(&j),
                        Some(&(j * 3)),
                        "n={n} lost {j} after removing {i}"
                    );
                }
            }
            assert!(pb.is_empty());
        }
    }

    #[test]
    fn round465_remove_in_reverse_order_empties_the_map() {
        // Descending removal drives the merge path from the other side —
        // rotate_from_left instead of rotate_from_right.
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..300_i64 {
            pb.insert_mut(i, i);
        }
        for i in (0..300_i64).rev() {
            assert_eq!(pb.remove_mut(&i), Some(i));
            check_map(&pb);
        }
        assert!(pb.is_empty());
    }

    #[test]
    fn round465_matches_btreemap_under_mixed_traffic() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        let mut model: BTreeMap<i64, i64> = BTreeMap::new();
        let mut seed = 0x5eed_1234_u64;
        for step in 0..4000 {
            let k = (lcg(&mut seed) % 300) as i64;
            if lcg(&mut seed) % 3 == 0 {
                assert_eq!(
                    pb.remove_mut(&k),
                    model.remove(&k),
                    "step {step} remove {k}"
                );
            } else {
                let v = (lcg(&mut seed) % 1000) as i64;
                assert_eq!(
                    pb.insert_mut(k, v),
                    model.insert(k, v),
                    "step {step} insert {k}"
                );
            }
            assert_eq!(pb.len(), model.len(), "step {step}");
            if step % 97 == 0 {
                check_map(&pb);
                let got: Vec<(i64, i64)> = pb.iter().map(|(k, v)| (*k, *v)).collect();
                let want: Vec<(i64, i64)> = model.iter().map(|(k, v)| (*k, *v)).collect();
                assert_eq!(got, want, "step {step}");
            }
        }
        check_map(&pb);
        let got: Vec<(i64, i64)> = pb.iter().map(|(k, v)| (*k, *v)).collect();
        let want: Vec<(i64, i64)> = model.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn round465_remove_leaves_a_shared_snapshot_untouched() {
        // The whole point of the persistent structure: a clone taken before
        // the removal must still see the removed key.
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..200_i64 {
            pb.insert_mut(i, i * 5);
        }
        let snapshot = pb.clone();
        for i in 0..100_i64 {
            pb.remove_mut(&(i * 2));
        }
        check_map(&pb);
        check_map(&snapshot);
        assert_eq!(snapshot.len(), 200);
        for i in 0..200_i64 {
            assert_eq!(snapshot.get(&i), Some(&(i * 5)), "snapshot lost {i}");
        }
        for i in 0..100_i64 {
            assert_eq!(pb.get(&(i * 2)), None);
            assert_eq!(pb.get(&(i * 2 + 1)), Some(&(i * 10 + 5)));
        }
    }

    #[test]
    fn round465_immutable_remove_does_not_touch_the_receiver() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..60_i64 {
            pb.insert_mut(i, i);
        }
        let (next, prev) = pb.remove(&30);
        assert_eq!(prev, Some(30));
        assert_eq!(pb.get(&30), Some(&30), "receiver must be untouched");
        assert_eq!(next.get(&30), None);
        assert_eq!(pb.len(), 60);
        assert_eq!(next.len(), 59);
        check_map(&pb);
        check_map(&next);
    }

    #[test]
    fn round465_range_and_predecessor_still_work_after_removals() {
        // Removal rewires spines; the ordered readers must follow.
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..400_i64 {
            pb.insert_mut(i, i);
        }
        for i in 0..400_i64 {
            if i % 3 == 0 {
                pb.remove_mut(&i);
            }
        }
        check_map(&pb);
        let in_range: Vec<i64> = pb
            .range(Bound::Included(&100), Bound::Excluded(&120))
            .map(|(k, _)| *k)
            .collect();
        let want: Vec<i64> = (100..120).filter(|i| i % 3 != 0).collect();
        assert_eq!(in_range, want);
        // 99 is a multiple of 3 and was removed, so 100's predecessor is 98.
        assert_eq!(pb.predecessor(&100).map(|(k, _)| *k), Some(98));
        let rev: Vec<i64> = pb.iter_rev().map(|(k, _)| *k).take(3).collect();
        assert_eq!(rev, vec![398, 397, 395]);
    }

    #[test]
    fn empty_map_is_empty() {
        let pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        assert_eq!(pb.len(), 0);
        assert!(pb.is_empty());
        assert!(pb.get(&42).is_none());
    }

    #[test]
    fn insert_single_into_empty_works() {
        let (pb, prev) = PersistentBTreeMap::<i64, i64>::new().insert(1, 100);
        assert_eq!(prev, None);
        assert_eq!(pb.len(), 1);
        assert_eq!(pb.get(&1), Some(&100));
        assert_eq!(pb.get(&2), None);
    }

    #[test]
    fn insert_replace_returns_prev_keeps_len() {
        let (pb, p1) = PersistentBTreeMap::<i64, i64>::new().insert(7, 10);
        assert_eq!(p1, None);
        let (pb, p2) = pb.insert(7, 99);
        assert_eq!(p2, Some(10));
        assert_eq!(pb.len(), 1);
        assert_eq!(pb.get(&7), Some(&99));
    }

    #[test]
    fn insert_crosses_leaf_split_boundary() {
        // 8 inserts cause the first leaf to split.
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..20_i64 {
            pb = pb.insert(i, i * 7).0;
        }
        for i in 0..20_i64 {
            assert_eq!(pb.get(&i), Some(&(i * 7)));
        }
        assert!(pb.get(&20).is_none());
        assert_eq!(pb.len(), 20);
    }

    #[test]
    fn insert_grows_through_multiple_internal_splits() {
        // 200 inserts force the trie depth to grow more than once.
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..200_i64 {
            pb = pb.insert(i, i * 11).0;
        }
        for i in 0..200_i64 {
            assert_eq!(pb.get(&i), Some(&(i * 11)));
        }
        assert_eq!(pb.len(), 200);
    }

    #[test]
    fn clone_then_insert_preserves_original() {
        let mut a: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..100_i64 {
            a = a.insert(i, i).0;
        }
        let b = a.clone();
        let (b, _) = b.insert(999, 999);
        assert_eq!(a.len(), 100);
        assert!(a.get(&999).is_none());
        assert_eq!(b.len(), 101);
        assert_eq!(b.get(&999), Some(&999));
        for i in 0..100_i64 {
            assert_eq!(a.get(&i), Some(&i), "A drift at {i}");
            assert_eq!(b.get(&i), Some(&i), "B drift at {i}");
        }
    }

    #[test]
    fn iter_yields_sorted_order() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        // Insert in shuffled order; iter must still come out sorted.
        for &k in &[7_i64, 3, 11, 1, 9, 5, 14, 2, 8, 12, 4, 6, 10, 13] {
            pb = pb.insert(k, k * 2).0;
        }
        let collected: Vec<(i64, i64)> = pb.iter().map(|(k, v)| (*k, *v)).collect();
        let expected: Vec<(i64, i64)> = (1..=14).map(|k| (k, k * 2)).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn iter_handles_taller_tree() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..500_i64 {
            pb = pb.insert(i, i).0;
        }
        let collected: Vec<i64> = pb.iter().map(|(k, _)| *k).collect();
        let expected: Vec<i64> = (0..500).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn range_basic_bounds() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..100_i64 {
            pb = pb.insert(i, i * 10).0;
        }
        let keys = |lo: Bound<&i64>, hi: Bound<&i64>| -> Vec<i64> {
            pb.range(lo, hi).map(|(k, _)| *k).collect()
        };
        assert_eq!(
            keys(Bound::Included(&20), Bound::Included(&24)),
            vec![20, 21, 22, 23, 24]
        );
        assert_eq!(
            keys(Bound::Excluded(&20), Bound::Excluded(&24)),
            vec![21, 22, 23]
        );
        assert_eq!(keys(Bound::Unbounded, Bound::Excluded(&3)), vec![0, 1, 2]);
        assert_eq!(
            keys(Bound::Included(&97), Bound::Unbounded),
            vec![97, 98, 99]
        );
        assert!(keys(Bound::Included(&50), Bound::Included(&49)).is_empty());
        // Out-of-range bounds clamp to the data.
        assert_eq!(
            keys(Bound::Included(&-5), Bound::Included(&2)),
            vec![0, 1, 2]
        );
        assert_eq!(
            keys(Bound::Included(&200), Bound::Unbounded),
            Vec::<i64>::new()
        );
    }

    /// Fuzz `range` against `BTreeMap::range` across random data + random
    /// bounds (inclusive / exclusive / unbounded on each end) — the perf
    /// index range scan rides on this, and it's a stone (max blast radius),
    /// so the range walk must match the std oracle exactly.
    #[test]
    fn fuzz_range_against_btreemap() {
        let mut rng = Splitmix::new(0x5EED_1234_u64);
        const KEY_RANGE: i64 = 512;
        // A few tree sizes so we exercise leaf-only, shallow, and deep trees.
        for &n_inserts in &[0usize, 5, 40, 300, 2000] {
            let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
            let mut oracle: BTreeMap<i64, i64> = BTreeMap::new();
            for _ in 0..n_inserts {
                let key = (rng.next() as i64).rem_euclid(KEY_RANGE);
                let val = rng.next() as i64;
                pb = pb.insert(key, val).0;
                oracle.insert(key, val);
            }
            for _ in 0..2000 {
                let a = (rng.next() as i64).rem_euclid(KEY_RANGE + 40) - 20;
                let b = (rng.next() as i64).rem_euclid(KEY_RANGE + 40) - 20;
                let (lo_raw, hi_raw) = if a <= b { (a, b) } else { (b, a) };
                let mk = |raw: i64, sel: u64| -> Bound<i64> {
                    match sel % 3 {
                        0 => Bound::Included(raw),
                        1 => Bound::Excluded(raw),
                        _ => Bound::Unbounded,
                    }
                };
                let lo = mk(lo_raw, rng.next());
                let hi = mk(hi_raw, rng.next());
                // `BTreeMap::range` panics on `Excluded(x)..Excluded(x)`; our
                // `range` yields empty there. Skip that one case for the oracle.
                if lo_raw == hi_raw
                    && matches!(lo, Bound::Excluded(_))
                    && matches!(hi, Bound::Excluded(_))
                {
                    continue;
                }
                let lo_ref = match &lo {
                    Bound::Included(k) => Bound::Included(k),
                    Bound::Excluded(k) => Bound::Excluded(k),
                    Bound::Unbounded => Bound::Unbounded,
                };
                let hi_ref = match &hi {
                    Bound::Included(k) => Bound::Included(k),
                    Bound::Excluded(k) => Bound::Excluded(k),
                    Bound::Unbounded => Bound::Unbounded,
                };
                let got: Vec<(i64, i64)> =
                    pb.range(lo_ref, hi_ref).map(|(k, v)| (*k, *v)).collect();
                let want: Vec<(i64, i64)> = oracle.range((lo, hi)).map(|(k, v)| (*k, *v)).collect();
                assert_eq!(
                    got, want,
                    "range drift n={n_inserts} lo={lo_raw:?} hi={hi_raw:?}"
                );
            }
        }
    }

    /// Fuzz `predecessor` against a `BTreeMap` oracle (largest key < probe)
    /// across leaf-only, shallow, and deep trees.
    #[test]
    fn fuzz_predecessor_against_btreemap() {
        let mut rng = Splitmix::new(0x9E37_79B9_7F4A_7C15_u64);
        const KEY_RANGE: i64 = 512;
        for &n_inserts in &[0usize, 5, 40, 300, 2000] {
            let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
            let mut oracle: BTreeMap<i64, i64> = BTreeMap::new();
            for _ in 0..n_inserts {
                let key = (rng.next() as i64).rem_euclid(KEY_RANGE);
                let val = rng.next() as i64;
                pb = pb.insert(key, val).0;
                oracle.insert(key, val);
            }
            for _ in 0..2000 {
                let probe = (rng.next() as i64).rem_euclid(KEY_RANGE + 40) - 20;
                let got = pb.predecessor(&probe).map(|(k, v)| (*k, *v));
                let want = oracle.range(..probe).next_back().map(|(k, v)| (*k, *v));
                assert_eq!(got, want, "predecessor drift n={n_inserts} probe={probe}");
            }
        }
    }

    #[test]
    fn iter_rev_yields_descending() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for &k in &[7_i64, 3, 11, 1, 9, 5, 14, 2, 8, 12, 4, 6, 10, 13] {
            pb = pb.insert(k, k * 2).0;
        }
        let collected: Vec<(i64, i64)> = pb.iter_rev().map(|(k, v)| (*k, *v)).collect();
        let expected: Vec<(i64, i64)> = (1..=14).rev().map(|k| (k, k * 2)).collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn iter_rev_handles_taller_tree() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..500_i64 {
            pb = pb.insert(i, i).0;
        }
        let collected: Vec<i64> = pb.iter_rev().map(|(k, _)| *k).collect();
        let expected: Vec<i64> = (0..500).rev().collect();
        assert_eq!(collected, expected);
    }

    #[test]
    fn iter_rev_empty_map_returns_nothing() {
        let pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        assert_eq!(pb.iter_rev().count(), 0);
    }

    #[test]
    fn iter_rev_lazy_stops_at_take() {
        // Critical for the ORDER BY DESC + LIMIT N executor path: only
        // the first N entries are touched, not the full N-entry walk.
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        for i in 0..10000_i64 {
            pb = pb.insert(i, i).0;
        }
        let top5: Vec<i64> = pb.iter_rev().take(5).map(|(k, _)| *k).collect();
        assert_eq!(top5, vec![9999, 9998, 9997, 9996, 9995]);
    }

    /// SplitMix-style PRNG so the fuzz oracle is reproducible.
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

    /// 100K-step random `insert` / `get` fuzz against `std::BTreeMap`.
    /// Validates split/merge/replace semantics across the full tree depth.
    #[test]
    fn fuzz_oracle_against_std_btreemap() {
        let mut pb: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        let mut oracle: BTreeMap<i64, i64> = BTreeMap::new();
        let mut rng = Splitmix::new(0xC0FFEE_u64);
        const STEPS: usize = 100_000;
        // Use a bounded key range so we hit replaces, not just inserts.
        const KEY_RANGE: i64 = 4096;
        for step in 0..STEPS {
            let op = rng.next() % 3; // 0/1: insert, 2: get-check
            let key = (rng.next() as i64) % KEY_RANGE;
            match op {
                0 | 1 => {
                    let val = rng.next() as i64;
                    let (new_pb, prev_pb) = pb.insert(key, val);
                    let prev_oracle = oracle.insert(key, val);
                    assert_eq!(prev_pb, prev_oracle, "prev drift @ step {step}, key {key}");
                    pb = new_pb;
                    assert_eq!(pb.len(), oracle.len(), "len drift @ step {step}");
                }
                2 => {
                    let pb_v = pb.get(&key).copied();
                    let oracle_v = oracle.get(&key).copied();
                    assert_eq!(pb_v, oracle_v, "get drift @ step {step}, key {key}");
                }
                _ => unreachable!(),
            }
        }
        // Final sweep: every key in the oracle must match.
        for (k, v) in &oracle {
            assert_eq!(pb.get(k), Some(v), "final drift at key {k}");
        }
        // And iter must produce the same sorted sequence.
        let pb_collected: Vec<(i64, i64)> = pb.iter().map(|(k, v)| (*k, *v)).collect();
        let oracle_collected: Vec<(i64, i64)> = oracle.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(pb_collected, oracle_collected);
    }

    /// Clone-isolation: branch A → B and C, mutate independently, verify
    /// each handle reads back its own state without leaking into others.
    #[test]
    fn fuzz_oracle_clone_isolation() {
        let mut a: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        let mut oracle_a: BTreeMap<i64, i64> = BTreeMap::new();
        let mut rng = Splitmix::new(0xDECAFBAD_u64);
        for _ in 0..1_000 {
            let k = (rng.next() as i64) % 1000;
            let v = rng.next() as i64;
            a = a.insert(k, v).0;
            oracle_a.insert(k, v);
        }
        // Branch.
        let mut b = a.clone();
        let mut oracle_b = oracle_a.clone();
        let mut c = a.clone();
        let mut oracle_c = oracle_a.clone();
        for _ in 0..500 {
            let k = (rng.next() as i64) % 2000;
            let v = rng.next() as i64;
            b = b.insert(k, v).0;
            oracle_b.insert(k, v);
        }
        for _ in 0..300 {
            let k = (rng.next() as i64) % 500;
            let v = rng.next() as i64;
            c = c.insert(k, v).0;
            oracle_c.insert(k, v);
        }
        for (k, v) in &oracle_a {
            assert_eq!(a.get(k), Some(v), "A drift at {k}");
        }
        for (k, v) in &oracle_b {
            assert_eq!(b.get(k), Some(v), "B drift at {k}");
        }
        for (k, v) in &oracle_c {
            assert_eq!(c.get(k), Some(v), "C drift at {k}");
        }
    }

    #[test]
    fn partial_eq_compares_by_elements() {
        let mut a: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        let mut b: PersistentBTreeMap<i64, i64> = PersistentBTreeMap::new();
        // Build the same end-state via different insertion orders → tree
        // shapes likely differ, but PartialEq compares by iter().
        for &k in &[5_i64, 2, 8, 1, 7, 3, 6, 4] {
            a = a.insert(k, k * 10).0;
        }
        for &k in &[1_i64, 2, 3, 4, 5, 6, 7, 8] {
            b = b.insert(k, k * 10).0;
        }
        assert_eq!(a, b);
        let (a, _) = a.insert(9, 90);
        assert_ne!(a, b);
    }
}
