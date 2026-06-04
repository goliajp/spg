//! v6.5.1 — per-distinct-SQL LRU stat collector.
//!
//! Tracks `(exec_count, total_us, max_us, last_seen_us)` per unique
//! SQL string. Bounded LRU cap of 1024 entries — when the cap is
//! exceeded the least-recently-recorded entry is evicted. Engine
//! calls `record(sql, elapsed_us, now_us)` after every successful
//! execute; the virtual table `spg_stat_query` reads the entries.
//!
//! Honest scope: SPG's plan cache (v6.3.0) lives at a different
//! layer — that one is keyed on SQL text too, but its purpose is
//! AST reuse, not observability. The query-stats layer is purely
//! introspection.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

/// Cap on distinct queries tracked. PG's pg_stat_statements
/// defaults to 5000; SPG ships 1024 because typical app workloads
/// reuse far fewer distinct statements. Configurable in v6.5.6.
pub(crate) const QUERY_STATS_MAX: usize = 1024;

#[derive(Debug, Clone, Default)]
pub struct QueryStat {
    pub exec_count: u64,
    pub total_us: u64,
    pub max_us: u64,
    pub last_seen_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    /// SQL string → stat counters. BTreeMap for deterministic
    /// iteration (the `spg_stat_query` virtual table needs stable
    /// row order across reads).
    entries: BTreeMap<String, QueryStat>,
    /// LRU order. Most-recently-recorded at the back. `record`
    /// touches this; `evict` pops the front.
    lru: VecDeque<String>,
}

impl QueryStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the recorded stat snapshot, if any. Does NOT promote
    /// LRU (introspection should be side-effect free).
    pub fn get(&self, sql: &str) -> Option<&QueryStat> {
        self.entries.get(sql)
    }

    /// Iterate every recorded entry in deterministic (BTreeMap)
    /// order. Used by `spg_stat_query` virtual table.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &QueryStat)> {
        self.entries.iter()
    }

    /// Record one execution. `elapsed_us` is the wall-clock micros
    /// between start and end; `now_us` is the wall-clock micros at
    /// completion (used for `last_seen_us`).
    pub fn record(&mut self, sql: &str, elapsed_us: u64, now_us: u64) {
        if let Some(stat) = self.entries.get_mut(sql) {
            stat.exec_count = stat.exec_count.saturating_add(1);
            stat.total_us = stat.total_us.saturating_add(elapsed_us);
            stat.max_us = stat.max_us.max(elapsed_us);
            stat.last_seen_us = now_us;
            // Promote to MRU in lru queue.
            if let Some(idx) = self.lru.iter().position(|k| k == sql) {
                let key = self.lru.remove(idx).expect("idx from position");
                self.lru.push_back(key);
            }
            return;
        }
        // New entry: enforce cap.
        if self.entries.len() >= QUERY_STATS_MAX
            && let Some(oldest) = self.lru.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            String::from(sql),
            QueryStat {
                exec_count: 1,
                total_us: elapsed_us,
                max_us: elapsed_us,
                last_seen_us: now_us,
            },
        );
        self.lru.push_back(String::from(sql));
    }

    /// v6.5.6 — operator-controlled clear (e.g. for ops resets).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    pub fn cap(&self) -> usize {
        QUERY_STATS_MAX
    }

    /// Snapshot rows in LRU order (oldest → newest). Used by
    /// `spg_stat_query` ORDER BY default.
    pub fn snapshot(&self) -> Vec<(String, QueryStat)> {
        self.lru
            .iter()
            .filter_map(|sql| self.entries.get(sql).map(|s| (sql.clone(), s.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn record_increments_counters() {
        let mut qs = QueryStats::new();
        qs.record("SELECT 1", 100, 1000);
        qs.record("SELECT 1", 200, 2000);
        let s = qs.get("SELECT 1").expect("present");
        assert_eq!(s.exec_count, 2);
        assert_eq!(s.total_us, 300);
        assert_eq!(s.max_us, 200);
        assert_eq!(s.last_seen_us, 2000);
    }

    #[test]
    fn distinct_sql_yields_separate_entries() {
        let mut qs = QueryStats::new();
        qs.record("SELECT 1", 10, 100);
        qs.record("SELECT 2", 20, 200);
        assert_eq!(qs.len(), 2);
    }

    #[test]
    fn lru_evicts_oldest_at_cap() {
        let mut qs = QueryStats::new();
        for i in 0..QUERY_STATS_MAX {
            qs.record(&alloc::format!("SELECT {i}"), 1, i as u64);
        }
        assert_eq!(qs.len(), QUERY_STATS_MAX);
        // Trigger cap.
        qs.record("SELECT new", 1, QUERY_STATS_MAX as u64);
        assert_eq!(qs.len(), QUERY_STATS_MAX);
        assert!(qs.get("SELECT 0").is_none(), "oldest evicted");
        assert!(qs.get("SELECT new").is_some());
    }

    #[test]
    fn re_recording_an_entry_promotes_lru() {
        let mut qs = QueryStats::new();
        qs.record("a", 1, 1);
        qs.record("b", 1, 2);
        qs.record("c", 1, 3);
        // Touch "a" — should become MRU.
        qs.record("a", 1, 4);
        // Fill to cap so the next insert evicts the LRU front.
        for i in 0..(QUERY_STATS_MAX - 3) {
            qs.record(&alloc::format!("filler{i}"), 1, 100 + i as u64);
        }
        qs.record("trigger", 1, 9999);
        assert!(qs.get("a").is_some(), "a was MRU; should survive");
        assert!(qs.get("b").is_none(), "b should be evicted");
    }

    #[test]
    fn clear_drops_everything() {
        let mut qs = QueryStats::new();
        qs.record("a", 1, 1);
        qs.record("b", 1, 2);
        qs.clear();
        assert!(qs.is_empty());
    }

    #[test]
    fn snapshot_returns_lru_order_oldest_first() {
        let mut qs = QueryStats::new();
        qs.record("a", 1, 100);
        qs.record("b", 1, 200);
        qs.record("c", 1, 300);
        let snap = qs.snapshot();
        let keys: Vec<String> = snap.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            keys,
            alloc::vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }
}
