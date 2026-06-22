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

/// v7.37.7 (mailrs cascade contention round 2 instrumentation) —
/// runtime counters to disambiguate samply attribution.
/// `samply` reported 67.8% CPU in `MemoizeCache::new` + drop_in_place
/// under 20-worker stress, but K01 (eager-alloc → lazy) didn't move
/// cascade amplification — suggesting either the attribution was off
/// or the eager 96 KB alloc isn't the dominant cost. These counters
/// provide ground truth: how many times is `new()` actually called,
/// how many entries does any cache ever hold, how many caches die
/// empty. Read once at end of bench via `MemoizeCache::counter_snapshot()`.
///
/// Counters use Relaxed atomic ops (visibility, no synchronization).
/// Cost per op is sub-ns and does not introduce contention.
pub mod counters {
    use core::sync::atomic::AtomicU64;

    pub static NEW_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static PUT_CALLS: AtomicU64 = AtomicU64::new(0);
    pub static MAX_ENTRIES_SEEN: AtomicU64 = AtomicU64::new(0);
    pub static DROP_WITH_ZERO_ENTRIES: AtomicU64 = AtomicU64::new(0);
    pub static DROP_WITH_ENTRIES: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Clone, Copy)]
    pub struct Snapshot {
        pub new_calls: u64,
        pub put_calls: u64,
        pub max_entries_seen: u64,
        pub drop_with_zero_entries: u64,
        pub drop_with_entries: u64,
    }

    pub fn snapshot() -> Snapshot {
        use core::sync::atomic::Ordering::Relaxed;
        Snapshot {
            new_calls: NEW_CALLS.load(Relaxed),
            put_calls: PUT_CALLS.load(Relaxed),
            max_entries_seen: MAX_ENTRIES_SEEN.load(Relaxed),
            drop_with_zero_entries: DROP_WITH_ZERO_ENTRIES.load(Relaxed),
            drop_with_entries: DROP_WITH_ENTRIES.load(Relaxed),
        }
    }

    pub fn reset() {
        use core::sync::atomic::Ordering::Relaxed;
        NEW_CALLS.store(0, Relaxed);
        PUT_CALLS.store(0, Relaxed);
        MAX_ENTRIES_SEEN.store(0, Relaxed);
        DROP_WITH_ZERO_ENTRIES.store(0, Relaxed);
        DROP_WITH_ENTRIES.store(0, Relaxed);
    }
}

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
    pub outer_values: Vec<Value<'static>>,
}

/// v7.29 - one batch-evaluated correlated subquery: the outer key
/// column and the key -> value map.
///
/// v7.37.x (docker-fair SCALARSQ attack) — extended with an
/// `empty_default` Value. PG scalar-subquery empty-set semantics
/// distinguish `COUNT(*)` / `COUNT(col)` (= 0 over no rows) from
/// every other aggregate (= NULL). The hollow_scalar_subqueries
/// template-rewrite step empties the inner SelectStatement before
/// the per-row splice, so the splicer cannot inspect the original
/// aggregate kind at probe time. Storing the empty-default on the
/// GroupMap captures that information at try_batch_correlated_scalar
/// construction time, where the original inner is still in hand.
pub type GroupMap = (
    spg_sql::ast::ColumnName,
    alloc::collections::BTreeMap<String, Value<'static>>,
    Value<'static>,
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

/// v7.34 (mailrs conn-pool-exhaustion P0) - decorrelated `[NOT] EXISTS`
/// semi/anti-join: the outer correlation columns (in key order) and the
/// set of encoded inner key-tuples that have >=1 matching inner row,
/// built in ONE scan. An outer row's EXISTS reduces to a membership
/// test, turning O(outer x inner-exec) per-row work into O(scan + outer
/// lookups) - PG's Hash Semi/Anti Join.
pub type ExistsSet = (
    alloc::vec::Vec<spg_sql::ast::ColumnName>,
    alloc::collections::BTreeSet<String>,
);

/// v7.30.2 (mailrs round-25) - canonicalised membership set for a
/// large all-literal `IN` list. Integer literals canonicalise to
/// i64 (cross-width `Int = BigInt` stays correct); string literals
/// stay verbatim. Mixed or exotic families are not eligible and
/// keep the linear `apply_binary` scan.
///
/// v7.37.x (docker-bench NOTEX 红线) — switched from `BTreeSet` to
/// `hashbrown::HashSet`. The probe shape is 25 k outer-row membership
/// lookups against a 12.5 k-element InList; BTreeSet was O(log N) ≈
/// 14 byte comparisons per lookup (~70 ns), hash set is O(1) ≈ 5 ns.
/// Net win on the docker-fair NOTEX bench: 4.4 ms → ~2 ms.
#[derive(Debug, Clone)]
pub enum InListSet {
    Int(hashbrown::HashSet<i64>),
    Text(hashbrown::HashSet<alloc::string::String>),
}

#[derive(Debug, Clone)]
pub struct InListSetEntry {
    pub set: InListSet,
    /// The list carried a NULL literal: a non-matching needle
    /// yields NULL, not FALSE (SQL three-valued logic).
    pub has_null: bool,
}

#[derive(Debug, Clone)]
pub struct MemoizeCache {
    /// LRU front = most recently used. Stored as a `VecDeque` so
    /// re-promoting a hit is `O(n)` worst-case but `O(1)`
    /// amortised for the common front-half-hit pattern of nested-
    /// loop correlated subqueries.
    entries: VecDeque<(CacheKey, Value<'static>)>,
    /// v7.29 (round-22 phase 3) - batch-evaluated correlated scalar
    /// subqueries: subquery repr -> Some((outer column, key -> value
    /// map built in ONE pass)) or None when the shape can't batch
    /// (so we don't re-analyse it per row). Turns 23.5k per-group
    /// executions into one grouped scan + 23.5k lookups.
    pub group_maps: alloc::collections::BTreeMap<String, Option<alloc::rc::Rc<GroupMap>>>,
    /// v7.37.x (docker-fair SCALARSQ attack) — fast-path cache keyed
    /// by `SelectStatement` pointer address. The repr-stringified
    /// `group_maps` key cost ~500 ns of `alloc::format!` per outer
    /// row; for hundreds-of-row LIMIT shapes that's still tens of µs
    /// of pure repr churn. Per-row hit is a HashMap probe on a usize
    /// key (the inner AST is stable for the SELECT's lifetime).
    pub group_maps_by_ptr: hashbrown::HashMap<usize, Option<alloc::rc::Rc<GroupMap>>>,
    /// v7.34 (mailrs conn-pool P0) - decorrelated `[NOT] EXISTS`: subquery
    /// repr -> Some(semi/anti-join key-set) or None when the shape can't
    /// decorrelate (don't re-analyse per row). Parallel to `group_maps`.
    pub exists_sets: alloc::collections::BTreeMap<String, Option<alloc::rc::Rc<ExistsSet>>>,
    /// v7.34.2 (EXISTS-FILTER baseline finding) — host-expression-ptr
    /// indexed plan: walk the WHERE expr ONCE, collect every EXISTS
    /// subquery in pre-order, build a decorrelated set for each, and
    /// store them as a `Vec` indexed by pre-order position. Per-row
    /// dispatch then walks the (cloned) expression in the same
    /// pre-order, increments an ordinal cursor, and reads the matching
    /// set out of this plan instead of re-running
    /// `alloc::format!("{subquery}")` and a fresh BTreeMap probe per
    /// row — the dominant cost of the 7.34.0 EXISTS-FILTER baseline.
    /// `None` slot = couldn't decorrelate that particular EXISTS; the
    /// dispatcher falls back to the legacy per-row resolver for it.
    pub exists_plans: alloc::collections::BTreeMap<usize, Vec<Option<alloc::rc::Rc<ExistsSet>>>>,
    /// v7.29 (3c) - host-expression ptr -> (subquery count, plan).
    pub expr_plans: alloc::collections::BTreeMap<usize, ExprPlan>,
    /// v7.30.2 (mailrs round-25) - InList node ptr -> membership set
    /// for large all-literal `IN` lists, built once per row loop.
    /// Turns the O(rows × list) membership scan into
    /// O(rows × log list). `None` = analysed, not eligible.
    pub in_sets: alloc::collections::BTreeMap<usize, Option<InListSetEntry>>,
    /// v7.30.2 (mailrs round-25) - host-expression ptr -> "contains
    /// a subquery node". The walk is O(tree) and a materialised IN
    /// list makes the tree huge — caching it makes the per-row
    /// dispatch O(log n) instead of O(24k list elements).
    pub has_subquery: alloc::collections::BTreeMap<usize, bool>,
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
        counters::NEW_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Self {
            entries: VecDeque::with_capacity(DEFAULT_MAX_ENTRIES),
            max_entries: DEFAULT_MAX_ENTRIES,
            max_bytes: DEFAULT_MAX_BYTES,
            current_bytes: 0,
            hit_count: 0,
            miss_count: 0,
            group_maps: alloc::collections::BTreeMap::new(),
            group_maps_by_ptr: hashbrown::HashMap::new(),
            exists_sets: alloc::collections::BTreeMap::new(),
            exists_plans: alloc::collections::BTreeMap::new(),
            expr_plans: alloc::collections::BTreeMap::new(),
            in_sets: alloc::collections::BTreeMap::new(),
            has_subquery: alloc::collections::BTreeMap::new(),
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
    pub fn get(&mut self, key: &CacheKey) -> Option<Value<'static>> {
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
    pub fn insert(&mut self, key: CacheKey, value: Value<'static>) {
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
        counters::PUT_CALLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let len = self.entries.len() as u64;
        counters::MAX_ENTRIES_SEEN.fetch_max(len, core::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for MemoizeCache {
    fn drop(&mut self) {
        use core::sync::atomic::Ordering::Relaxed;
        if self.entries.is_empty() {
            counters::DROP_WITH_ZERO_ENTRIES.fetch_add(1, Relaxed);
        } else {
            counters::DROP_WITH_ENTRIES.fetch_add(1, Relaxed);
        }
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

    fn key(repr: &str, outer: &[Value<'static>]) -> CacheKey {
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
            c.insert(key("q", &[Value::Int(i)]), Value::text(big_str));
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
