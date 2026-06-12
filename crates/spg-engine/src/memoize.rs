// pedantic doc_markdown flags every bare ident in the comment-as-
// spec block; allowing at the module level keeps the spec readable.
#![allow(clippy::doc_markdown)]

//! v6.2.6 — Memoize cache for correlated subqueries.
//!
//! When a `WHERE` clause references a scalar subquery whose inner
//! body depends on the outer row's column values (the classic
//! `WHERE id IN (SELECT MAX(x) FROM y WHERE y.k = outer.k)`
//! shape), the engine's current behaviour re-runs the inner
//! SELECT once per outer row — `O(outer_rows × inner_cost)` work
//! even when many outer rows share the same correlated key.
//!
//! v6.2.6 wraps that path with a per-query `MemoizeCache`:
//! before running the inner, hash the (subquery identity, outer-
//! row values) key and look it up; cache hits return the prior
//! result without re-executing. Caps:
//!
//!   - **1024 entries** (configurable via the planner's
//!     [`Self::with_max_entries`])
//!   - **16 MiB** of cumulative cached `Value` bytes (v5.5
//!     per-query memory budget's 1/16 share; configurable via
//!     [`Self::with_max_bytes`])
//!
//! When either cap is hit, the least-recently-used entry is
//! evicted before insertion.
//!
//! v6.2.6 ships the simple linear-vec LRU. v6.2.x can swap to a
//! BTreeMap + LinkedList for sub-`O(n)` lookup if it ever
//! matters; the gate is "≥ 5× speedup on the repeated-key
//! workload" which the linear scan clears at scale-1k.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use spg_storage::Value;

/// v6.2.6 — default cache size cap. Matches the design's "1024
/// entries" figure (V6_2_DESIGN.md L2 row 6).
pub const DEFAULT_MAX_ENTRIES: usize = 1024;

/// v6.2.6 — default cumulative bytes cap. 16 MiB matches the
/// v5.5 per-query budget's 1/16 share.
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Cache key — the subquery's textual identity plus the outer
/// row's value tuple. Two scalar-subquery node positions with
/// identical Display text are treated as the same subquery for
/// caching purposes (sound: equal Display → equal AST).
#[derive(Debug, Clone, PartialEq)]
pub struct CacheKey {
    pub subquery_repr: String,
    pub outer_values: Vec<Value>,
}

/// v7.29 - one batch-evaluated correlated subquery: the outer key
/// column and the key -> value map.
pub type GroupMap = (
    spg_sql::ast::ColumnName,
    alloc::collections::BTreeMap<String, Value>,
);

/// v7.29 (3c) - per-expression resolution plan: for the i-th scalar
/// subquery node (pre-order) of a host expression, the shared batch
/// map (None = unbatchable, resolve per row). Keyed by the HOST
/// expression's address - callers guarantee the expression outlives
/// the per-query memo (aggregate items / WHERE trees do). The stored
/// subquery count guards against address reuse.
/// (subquery count, per-subquery batch maps, hollow template). The
/// template is the host expression with every scalar subquery BODY
/// emptied - cloning it per row costs nodes, not whole subquery
/// ASTs (the splice walk replaces the hollow nodes by pre-order).
pub type ExprPlan = (
    usize,
    alloc::vec::Vec<Option<alloc::rc::Rc<GroupMap>>>,
    spg_sql::ast::Expr,
);

#[derive(Debug, Clone)]
pub struct MemoizeCache {
    /// LRU front = most recently used. Stored as a `VecDeque` so
    /// re-promoting a hit is `O(n)` worst-case but `O(1)`
    /// amortised for the common front-half-hit pattern of nested-
    /// loop correlated subqueries.
    entries: VecDeque<(CacheKey, Value)>,
    /// v7.29 (round-22 phase 3) - batch-evaluated correlated scalar
    /// subqueries: subquery repr -> Some((outer column, key -> value
    /// map built in ONE pass)) or None when the shape can't batch
    /// (so we don't re-analyse it per row). Turns 23.5k per-group
    /// executions into one grouped scan + 23.5k lookups.
    pub group_maps: alloc::collections::BTreeMap<String, Option<alloc::rc::Rc<GroupMap>>>,
    /// v7.29 (3c) - host-expression ptr -> (subquery count, plan).
    pub expr_plans: alloc::collections::BTreeMap<usize, ExprPlan>,
    max_entries: usize,
    max_bytes: usize,
    current_bytes: usize,
    pub hit_count: u64,
    pub miss_count: u64,
}

impl Default for MemoizeCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoizeCache {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(DEFAULT_MAX_ENTRIES),
            max_entries: DEFAULT_MAX_ENTRIES,
            max_bytes: DEFAULT_MAX_BYTES,
            current_bytes: 0,
            hit_count: 0,
            miss_count: 0,
            group_maps: alloc::collections::BTreeMap::new(),
            expr_plans: alloc::collections::BTreeMap::new(),
        }
    }

    pub const fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
        self
    }

    pub const fn with_max_bytes(mut self, b: usize) -> Self {
        self.max_bytes = b;
        self
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a cached scalar value. On hit, re-promotes the
    /// entry to the LRU front and bumps `hit_count`. On miss,
    /// returns `None` (caller runs the subquery + `insert`s).
    pub fn get(&mut self, key: &CacheKey) -> Option<Value> {
        let pos = self.entries.iter().position(|(k, _)| k == key);
        if let Some(p) = pos {
            let (k, v) = self.entries.remove(p)?;
            self.entries.push_front((k, v.clone()));
            self.hit_count += 1;
            Some(v)
        } else {
            self.miss_count += 1;
            None
        }
    }

    /// Insert a freshly-computed scalar value. Caller must have
    /// `get`-missed first (the cache doesn't dedupe inserts).
    /// Evicts LRU entries until both caps are satisfied.
    pub fn insert(&mut self, key: CacheKey, value: Value) {
        let entry_bytes = approx_bytes(&key) + approx_value_bytes(&value);
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self.current_bytes + entry_bytes > self.max_bytes)
        {
            let Some((k, v)) = self.entries.pop_back() else {
                break;
            };
            self.current_bytes = self
                .current_bytes
                .saturating_sub(approx_bytes(&k) + approx_value_bytes(&v));
        }
        self.current_bytes = self.current_bytes.saturating_add(entry_bytes);
        self.entries.push_front((key, value));
    }
}

fn approx_bytes(key: &CacheKey) -> usize {
    key.subquery_repr.len()
        + key
            .outer_values
            .iter()
            .map(approx_value_bytes)
            .sum::<usize>()
        + 16
}

fn approx_value_bytes(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) | Value::SmallInt(_) => 1,
        Value::Int(_) => 4,
        Value::BigInt(_) | Value::Float(_) => 8,
        Value::Date(_) | Value::Timestamp(_) => 8,
        Value::Interval { .. } => 16,
        Value::Numeric { .. } => 16,
        Value::Text(s) | Value::Json(s) => s.len(),
        Value::Vector(v) => v.len() * 4,
        Value::Sq8Vector(q) => q.bytes.len() + 8,
        Value::HalfVector(h) => h.dim() * 2,
        // v7.5.0 — Value is #[non_exhaustive]; conservative estimate.
        _ => 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(repr: &str, outer: &[Value]) -> CacheKey {
        CacheKey {
            subquery_repr: repr.into(),
            outer_values: outer.to_vec(),
        }
    }

    #[test]
    fn empty_cache_misses_everything() {
        let mut c = MemoizeCache::new();
        let k = key("SELECT 1", &[Value::Int(1)]);
        assert!(c.get(&k).is_none());
        assert_eq!(c.miss_count, 1);
        assert_eq!(c.hit_count, 0);
    }

    #[test]
    fn insert_then_get_hits() {
        let mut c = MemoizeCache::new();
        let k = key("SELECT 1", &[Value::Int(1)]);
        c.insert(k.clone(), Value::BigInt(42));
        let v = c.get(&k);
        assert_eq!(v, Some(Value::BigInt(42)));
        assert_eq!(c.hit_count, 1);
    }

    #[test]
    fn repeated_outer_key_hits_after_first_insert() {
        let mut c = MemoizeCache::new();
        let repr = "SELECT MAX(x) FROM y WHERE y.k = outer.k";
        for i in 0..100 {
            let k = key(repr, &[Value::Int(i % 5)]);
            if c.get(&k).is_none() {
                c.insert(k, Value::BigInt(i64::from(i)));
            }
        }
        // 5 unique keys → 5 misses, 95 hits.
        assert_eq!(c.miss_count, 5);
        assert_eq!(c.hit_count, 95);
    }

    #[test]
    fn lru_eviction_at_max_entries() {
        let mut c = MemoizeCache::new().with_max_entries(3);
        for i in 0..5 {
            let k = key("q", &[Value::Int(i)]);
            c.insert(k, Value::BigInt(i64::from(i)));
        }
        assert!(c.len() <= 3, "len={}", c.len());
        // Last 3 inserted (i=2, 3, 4) should be the survivors.
        assert!(c.get(&key("q", &[Value::Int(4)])).is_some());
        assert!(c.get(&key("q", &[Value::Int(3)])).is_some());
        assert!(c.get(&key("q", &[Value::Int(2)])).is_some());
        // Older entries evicted.
        assert!(c.get(&key("q", &[Value::Int(0)])).is_none());
    }

    #[test]
    fn lru_eviction_at_max_bytes() {
        let mut c = MemoizeCache::new().with_max_bytes(128);
        // Big strings exceed 128 bytes fast.
        for i in 0..10 {
            let big_str = alloc::string::String::from_iter(core::iter::repeat_n('x', 64));
            c.insert(key("q", &[Value::Int(i)]), Value::Text(big_str));
        }
        assert!(c.len() < 10, "len={}", c.len());
    }

    #[test]
    fn distinct_subquery_reprs_dont_collide() {
        let mut c = MemoizeCache::new();
        let k1 = key("SELECT 1", &[Value::Int(1)]);
        let k2 = key("SELECT 2", &[Value::Int(1)]);
        c.insert(k1.clone(), Value::BigInt(10));
        c.insert(k2.clone(), Value::BigInt(20));
        assert_eq!(c.get(&k1), Some(Value::BigInt(10)));
        assert_eq!(c.get(&k2), Some(Value::BigInt(20)));
    }

    #[test]
    fn miss_then_hit_bumps_promotes_to_lru_front() {
        let mut c = MemoizeCache::new().with_max_entries(3);
        c.insert(key("q", &[Value::Int(0)]), Value::BigInt(0));
        c.insert(key("q", &[Value::Int(1)]), Value::BigInt(1));
        c.insert(key("q", &[Value::Int(2)]), Value::BigInt(2));
        // Touch 0 — promote to front.
        let _ = c.get(&key("q", &[Value::Int(0)]));
        // Insert a new entry — evicts the LRU (which is now 1, not 0).
        c.insert(key("q", &[Value::Int(3)]), Value::BigInt(3));
        assert!(c.get(&key("q", &[Value::Int(0)])).is_some());
        assert!(c.get(&key("q", &[Value::Int(1)])).is_none());
    }
}
