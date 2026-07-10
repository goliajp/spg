//! v6.3.0 — Engine-level plan cache.
//!
//! Caches the post-`prepare()` `Statement` (clock-rewritten,
//! ORDER-BY-position-resolved, JOIN-reordered) keyed on the raw SQL
//! string. Hit path skips parse + clock rewrite + JOIN reorder — for
//! a 5-table JOIN that's the dominant cost.
//!
//! `statistics_version` and `source_tables` are stored on the entry
//! so v6.3.1 can invalidate selectively when ANALYZE bumps the stats
//! version, or when DDL changes one of the source tables.
//!
//! `describe_columns` is reserved for v6.3.3 Describe pre-Execute —
//! v6.3.0 leaves it empty.
//!
//! Cache is bounded by `PLAN_CACHE_MAX_ENTRIES` (256). Eviction is
//! LRU via a `VecDeque<String>` move-to-back on get. Both `get` and
//! `insert` are sub-microsecond at 256 entries.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use spg_sql::ast::{Expr, FromClause, FromJoin, SelectItem, SelectStatement, Statement, TableRef};
use spg_storage::ColumnSchema;

/// Hard cap on plan-cache entries. At 256 the cap holds the typical
/// app's reusable statement set without unbounded growth; at average
/// 4 KiB per cached AST the worst-case footprint is 1 MiB per
/// Engine. NOT a frozen surface — v6.3.x can re-tune.
pub(crate) const PLAN_CACHE_MAX_ENTRIES: usize = 256;

/// One cached plan. The cached `stmt` is the same one
/// `Engine::prepare()` would return — parse + clock rewrite +
/// ORDER-BY position resolution + JOIN reorder all already applied.
#[derive(Debug, Clone)]
pub struct PreparedPlan {
    pub stmt: Statement,
    /// Statistics version snapshot at prepare time. v6.3.1 compares
    /// this against the live statistics version and evicts on
    /// mismatch. v6.3.0 stores it but doesn't consult on lookup.
    pub statistics_version: u64,
    /// Tables referenced by `stmt` (deduplicated, lexical order).
    /// v6.3.1 uses this for selective DDL/ANALYZE invalidation.
    pub source_tables: Vec<String>,
    /// Column shape v6.3.3 will populate for `Describe statement`.
    /// v6.3.0 leaves this empty.
    pub describe_columns: Vec<ColumnSchema>,
}

#[derive(Debug, Clone)]
pub struct PlanCache {
    /// SQL string → cached plan. `BTreeMap` for deterministic
    /// iteration (test stability); ordering of LRU is tracked
    /// separately in `lru`.
    entries: BTreeMap<String, PreparedPlan>,
    /// LRU queue. Newest entry at the back. `get` moves the
    /// referenced key to the back; `insert` pushes to the back and
    /// evicts the front when at cap.
    lru: VecDeque<String>,
    /// v6.5.6 — runtime-configurable cap. Defaults to
    /// `PLAN_CACHE_MAX_ENTRIES` (256); spg-server reads
    /// `SPG_PLAN_CACHE_MAX` env at startup and overrides via
    /// `PlanCache::with_max_entries`.
    max_entries: usize,
}

impl Default for PlanCache {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            lru: VecDeque::new(),
            max_entries: PLAN_CACHE_MAX_ENTRIES,
        }
    }
}

impl PlanCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// v6.5.6 — runtime cap override. Operator-tunable via
    /// `SPG_PLAN_CACHE_MAX` env at startup. Minimum 1; values
    /// above the compile-time `PLAN_CACHE_MAX_ENTRIES` are
    /// clamped down to it (defensive backstop against runaway
    /// configs).
    pub fn set_max_entries(&mut self, n: usize) {
        self.max_entries = n.max(1).min(PLAN_CACHE_MAX_ENTRIES);
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Read-only peek without LRU promotion. Used by introspection
    /// and v6.3.1 tests that want to inspect a cached plan's
    /// metadata without mutating the cache.
    pub fn get_snapshot(&self, sql: &str) -> Option<&PreparedPlan> {
        self.entries.get(sql)
    }

    /// Returns the cached plan if present and promotes it to most-
    /// recently-used. Returns `None` on miss.
    pub fn get(&mut self, sql: &str) -> Option<&PreparedPlan> {
        if !self.entries.contains_key(sql) {
            return None;
        }
        if let Some(idx) = self.lru.iter().position(|k| k == sql) {
            let key = self.lru.remove(idx).expect("idx came from position()");
            self.lru.push_back(key);
        }
        self.entries.get(sql)
    }

    /// Inserts (or replaces) the plan. Evicts the oldest entry if
    /// we'd exceed `PLAN_CACHE_MAX_ENTRIES`.
    pub fn insert(&mut self, sql: String, plan: PreparedPlan) {
        if self.entries.contains_key(&sql) {
            if let Some(idx) = self.lru.iter().position(|k| k == &sql) {
                let key = self.lru.remove(idx).expect("idx came from position()");
                self.lru.push_back(key);
            }
            self.entries.insert(sql, plan);
            return;
        }
        if self.entries.len() >= self.max_entries {
            if let Some(oldest) = self.lru.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.lru.push_back(sql.clone());
        self.entries.insert(sql, plan);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    /// v6.3.1 will use this for explicit invalidation. v6.3.0
    /// exposes it for tests + future use.
    pub fn evict(&mut self, sql: &str) -> Option<PreparedPlan> {
        let plan = self.entries.remove(sql)?;
        if let Some(idx) = self.lru.iter().position(|k| k == sql) {
            self.lru.remove(idx);
        }
        Some(plan)
    }

    /// v6.3.1 will use this to evict every plan that references a
    /// specific table.
    pub fn evict_referencing(&mut self, table: &str) -> usize {
        let to_evict: Vec<String> = self
            .entries
            .iter()
            .filter_map(|(k, p)| {
                if p.source_tables.iter().any(|t| t == table) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        let n = to_evict.len();
        for k in to_evict {
            self.entries.remove(&k);
            if let Some(idx) = self.lru.iter().position(|x| x == &k) {
                self.lru.remove(idx);
            }
        }
        n
    }
}

/// Walk a `Statement` and collect every distinct table name referenced
/// by its FROM clauses (including JOIN tables and subquery FROMs).
/// Used by `PreparedPlan::source_tables` for v6.3.1 selective
/// invalidation.
pub fn collect_source_tables(stmt: &Statement) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match stmt {
        Statement::Select(s) => collect_from_select(s, &mut out),
        Statement::Insert(s) => push_unique(&mut out, &s.table),
        Statement::Update(s) => {
            push_unique(&mut out, &s.table);
            if let Some(w) = &s.where_ {
                collect_expr(w, &mut out);
            }
        }
        Statement::Delete(s) => {
            push_unique(&mut out, &s.table);
            if let Some(w) = &s.where_ {
                collect_expr(w, &mut out);
            }
        }
        Statement::Explain(inner) => {
            collect_from_select(&inner.inner, &mut out);
        }
        _ => {}
    }
    out.sort();
    out.dedup();
    out
}

fn collect_from_select(s: &SelectStatement, out: &mut Vec<String>) {
    if let Some(from) = &s.from {
        collect_from_clause(from, out);
    }
    if let Some(w) = &s.where_ {
        collect_expr(w, out);
    }
    if let Some(h) = &s.having {
        collect_expr(h, out);
    }
    for item in &s.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_expr(expr, out);
        }
    }
    for (_, peer) in &s.unions {
        collect_from_select(peer, out);
    }
}

fn collect_from_clause(from: &FromClause, out: &mut Vec<String>) {
    collect_table_ref(&from.primary, out);
    for j in &from.joins {
        collect_from_join(j, out);
    }
}

fn collect_from_join(j: &FromJoin, out: &mut Vec<String>) {
    collect_table_ref(&j.table, out);
    if let Some(on) = &j.on {
        collect_expr(on, out);
    }
}

fn collect_table_ref(t: &TableRef, out: &mut Vec<String>) {
    push_unique(out, &t.name);
}

fn collect_expr(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::AggregateOrdered {
            call,
            order_by,
            filter,
            ..
        } => {
            collect_expr(call, out);
            for o in order_by {
                collect_expr(&o.expr, out);
            }
            // A `FILTER (WHERE …)` predicate can carry a subquery that
            // names a table; track it so writes to that table still
            // invalidate this cached plan.
            if let Some(f) = filter {
                collect_expr(f, out);
            }
        }
        Expr::ScalarSubquery(inner) => collect_from_select(inner, out),
        Expr::Exists { subquery, .. } => collect_from_select(subquery, out),
        Expr::InSubquery { expr, subquery, .. } => {
            collect_expr(expr, out);
            collect_from_select(subquery, out);
        }
        Expr::RowInSubquery { row, subquery, .. } => {
            for el in row {
                collect_expr(el, out);
            }
            collect_from_select(subquery, out);
        }
        Expr::RowCmpSubquery { row, subquery, .. } => {
            for el in row {
                collect_expr(el, out);
            }
            collect_from_select(subquery, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr(lhs, out);
            collect_expr(rhs, out);
        }
        Expr::Unary { expr, .. } => collect_expr(expr, out),
        Expr::Cast { expr, .. } | Expr::FieldAccess { base: expr, .. } => collect_expr(expr, out),
        Expr::IsNull { expr, .. } => collect_expr(expr, out),
        Expr::Like { expr, pattern, .. } => {
            collect_expr(expr, out);
            collect_expr(pattern, out);
        }
        Expr::FunctionCall { args, .. } => {
            for a in args {
                collect_expr(a, out);
            }
        }
        Expr::WindowFunction {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for a in args {
                collect_expr(a, out);
            }
            for p in partition_by {
                collect_expr(p, out);
            }
            for (o, _, _) in order_by {
                collect_expr(o, out);
            }
        }
        Expr::Extract { source, .. } => collect_expr(source, out),
        Expr::Array(items) => {
            for elem in items {
                collect_expr(elem, out);
            }
        }
        Expr::ArraySubscript { target, index } => {
            collect_expr(target, out);
            collect_expr(index, out);
        }
        Expr::ArraySlice { target, lo, hi } => {
            collect_expr(target, out);
            if let Some(l) = lo {
                collect_expr(l, out);
            }
            if let Some(h) = hi {
                collect_expr(h, out);
            }
        }
        Expr::AnyAll { expr, array, .. } => {
            collect_expr(expr, out);
            collect_expr(array, out);
        }
        Expr::InList { expr, list, .. } => {
            collect_expr(expr, out);
            for item in list {
                collect_expr(item, out);
            }
        }
        Expr::Case {
            operand,
            branches,
            else_branch,
        } => {
            if let Some(o) = operand {
                collect_expr(o, out);
            }
            for (w, t) in branches {
                collect_expr(w, out);
                collect_expr(t, out);
            }
            if let Some(e) = else_branch {
                collect_expr(e, out);
            }
        }
        Expr::Literal(_) | Expr::Column(_) | Expr::Placeholder(_) => {}
    }
}

fn push_unique(out: &mut Vec<String>, s: &str) {
    if !out.iter().any(|x| x == s) {
        out.push(String::from(s));
    }
}

// ── unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use spg_sql::parser::parse_statement;

    fn dummy_plan(version: u64, tables: &[&str]) -> PreparedPlan {
        let stmt = parse_statement("SELECT 1").expect("trivial SELECT parses");
        PreparedPlan {
            stmt,
            statistics_version: version,
            source_tables: tables.iter().map(|s| s.to_string()).collect(),
            describe_columns: Vec::new(),
        }
    }

    #[test]
    fn new_cache_is_empty() {
        let cache = PlanCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn insert_then_get_returns_the_plan() {
        let mut cache = PlanCache::new();
        cache.insert("SELECT 1".into(), dummy_plan(0, &["t"]));
        assert_eq!(cache.len(), 1);
        let plan = cache.get("SELECT 1").expect("hit");
        assert_eq!(plan.source_tables, alloc::vec!["t".to_string()]);
    }

    #[test]
    fn miss_returns_none() {
        let mut cache = PlanCache::new();
        cache.insert("SELECT 1".into(), dummy_plan(0, &[]));
        assert!(cache.get("SELECT 2").is_none());
    }

    #[test]
    fn replace_overwrites_existing_entry() {
        let mut cache = PlanCache::new();
        cache.insert("SELECT 1".into(), dummy_plan(1, &["a"]));
        cache.insert("SELECT 1".into(), dummy_plan(2, &["b"]));
        assert_eq!(cache.len(), 1);
        let plan = cache.get("SELECT 1").expect("hit");
        assert_eq!(plan.statistics_version, 2);
    }

    #[test]
    fn lru_evicts_oldest_at_cap() {
        let mut cache = PlanCache::new();
        for i in 0..PLAN_CACHE_MAX_ENTRIES {
            cache.insert(alloc::format!("SELECT {i}"), dummy_plan(i as u64, &[]));
        }
        assert_eq!(cache.len(), PLAN_CACHE_MAX_ENTRIES);
        cache.insert("SELECT new".into(), dummy_plan(999, &[]));
        assert_eq!(cache.len(), PLAN_CACHE_MAX_ENTRIES);
        assert!(cache.get("SELECT 0").is_none());
        assert!(cache.get("SELECT new").is_some());
    }

    #[test]
    fn get_promotes_lru_position() {
        let mut cache = PlanCache::new();
        cache.insert("a".into(), dummy_plan(0, &[]));
        cache.insert("b".into(), dummy_plan(0, &[]));
        cache.insert("c".into(), dummy_plan(0, &[]));
        // Touch "a" to make it MRU.
        let _ = cache.get("a");
        // Fill to cap so the next insert evicts. After we touched "a",
        // "b" should be the oldest now.
        for i in 0..(PLAN_CACHE_MAX_ENTRIES - 3) {
            cache.insert(alloc::format!("filler{i}"), dummy_plan(0, &[]));
        }
        cache.insert("trigger".into(), dummy_plan(0, &[]));
        assert!(
            cache.get("a").is_some(),
            "a was MRU after get(); should survive"
        );
        assert!(cache.get("b").is_none(), "b should be evicted");
    }

    #[test]
    fn clear_drops_everything() {
        let mut cache = PlanCache::new();
        cache.insert("a".into(), dummy_plan(0, &[]));
        cache.insert("b".into(), dummy_plan(0, &[]));
        cache.clear();
        assert!(cache.is_empty());
        assert!(cache.get("a").is_none());
    }

    #[test]
    fn evict_referencing_drops_only_matching_plans() {
        let mut cache = PlanCache::new();
        cache.insert("a".into(), dummy_plan(0, &["users"]));
        cache.insert("b".into(), dummy_plan(0, &["orders"]));
        cache.insert("c".into(), dummy_plan(0, &["users", "orders"]));
        let n = cache.evict_referencing("users");
        assert_eq!(n, 2);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_none());
    }

    #[test]
    fn collect_source_tables_from_simple_select() {
        let stmt = parse_statement("SELECT a, b FROM t1 WHERE x = 1").expect("parses");
        let tables = collect_source_tables(&stmt);
        assert_eq!(tables, alloc::vec!["t1".to_string()]);
    }

    #[test]
    fn collect_source_tables_from_join() {
        let stmt =
            parse_statement("SELECT * FROM t1 JOIN t2 ON t1.a = t2.b JOIN t3 ON t2.c = t3.d")
                .expect("parses");
        let tables = collect_source_tables(&stmt);
        assert_eq!(
            tables,
            alloc::vec!["t1".to_string(), "t2".to_string(), "t3".to_string()]
        );
    }

    #[test]
    fn collect_source_tables_dedupes_self_join() {
        let stmt = parse_statement("SELECT * FROM t1 a JOIN t1 b ON a.x = b.y").expect("parses");
        let tables = collect_source_tables(&stmt);
        assert_eq!(tables, alloc::vec!["t1".to_string()]);
    }
}
