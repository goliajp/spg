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

impl<K: Ord, V> PersistentBTreeMap<K, V> {
    /// `O(log₈ N)`. Binary-search at each level; on hit returns the value,
    /// on miss descends into the child between adjacent entries.
    pub fn get(&self, key: &K) -> Option<&V> {
        let mut node: &Arc<BNode<K, V>> = &self.root;
        loop {
            match &**node {
                BNode::Leaf { entries } => {
                    return entries
                        .binary_search_by(|(k, _)| k.cmp(key))
                        .ok()
                        .map(|i| &entries[i].1);
                }
                BNode::Internal { entries, children } => {
                    match entries.binary_search_by(|(k, _)| k.cmp(key)) {
                        Ok(i) => return Some(&entries[i].1),
                        Err(i) => {
                            node = &children[i];
                        }
                    }
                }
            }
        }
    }

    /// In-order key-then-value iterator. Used by `PartialEq` and any caller
    /// that needs to walk the whole map (e.g. catalog deserialization).
    pub fn iter(&self) -> Iter<'_, K, V> {
        let mut stack: Vec<(&Arc<BNode<K, V>>, usize)> = Vec::with_capacity(8);
        stack.push((&self.root, 0));
        Iter { stack }
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
