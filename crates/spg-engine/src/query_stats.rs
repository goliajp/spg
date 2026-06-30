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
    /// v7.37.22 (22.9) — cumulative row count produced / affected
    /// across every execution of this normalised template. Matches
    /// PG `pg_stat_statements.rows`. SELECT counts the result row
    /// count; INSERT/UPDATE/DELETE count `affected`. Saturating add.
    pub total_rows: u64,
    /// v7.37.22 (22.9) — peak per-execution row count. Useful for
    /// dashboards flagging templates whose worst case is a runaway
    /// scan (e.g. a SELECT that usually returns 10 rows but
    /// occasionally pulls 10M).
    pub max_rows: u64,
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
    ///
    /// v7.37.22 (22.6) — `sql` is normalised before lookup so
    /// callers don't have to know about the normalisation rules.
    pub fn get(&self, sql: &str) -> Option<&QueryStat> {
        let key = Self::normalize_sql(sql);
        self.entries.get(&key)
    }

    /// Iterate every recorded entry in deterministic (BTreeMap)
    /// order. Used by `spg_stat_query` virtual table.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &QueryStat)> {
        self.entries.iter()
    }

    /// v7.37.22 (22.6) — normalise a SQL string for pg_stat_statements
    /// grouping. Replaces literal values with `$N` placeholders so
    /// `SELECT * FROM t WHERE id = 1` and `SELECT * FROM t WHERE id
    /// = 2` collapse into the same key (matching PG's behaviour).
    ///
    /// Rules:
    /// - Numeric literals (integer + float) → `$N`
    /// - Single-quoted string literals (with `''` escape) → `$N`
    /// - NULL / TRUE / FALSE → preserved (PG also preserves these)
    /// - Whitespace runs → single space
    /// - Comments stripped (`-- …` to EOL; `/* … */` block)
    ///
    /// Each replaced literal increments `N` so multi-literal
    /// queries get `$1, $2, $3`. Round-trippable enough that DBAs
    /// reading the normalised form can map it back to the original
    /// query template.
    pub fn normalize_sql(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        let bytes = sql.as_bytes();
        let mut i = 0usize;
        let mut param_counter: u32 = 0;
        let mut last_was_space = true; // suppress leading space
        while i < bytes.len() {
            let b = bytes[i];
            // Line comment.
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            // Block comment.
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = i.saturating_add(2).min(bytes.len());
                continue;
            }
            // Whitespace collapse.
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
                i += 1;
                continue;
            }
            // String literal (single-quoted) with PG-style `''` escape.
            if b == b'\'' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2; // escaped quote inside the literal
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                param_counter += 1;
                out.push('$');
                let _ = core::fmt::Write::write_fmt(
                    &mut out,
                    format_args!("{param_counter}"),
                );
                last_was_space = false;
                continue;
            }
            // Numeric literal. PG considers digits, an optional
            // leading sign (already separated by the prev token),
            // a decimal point, an optional exponent (`e±NN`). The
            // safe boundary: previous emitted char is NOT an
            // identifier char (`[A-Za-z0-9_]`). When the previous
            // char IS an ident char, treat the digits as part of
            // an identifier (e.g. `col42`).
            if b.is_ascii_digit() {
                let prev_is_ident = out
                    .as_bytes()
                    .last()
                    .map(|c| {
                        c.is_ascii_alphanumeric() || *c == b'_'
                    })
                    .unwrap_or(false);
                if !prev_is_ident {
                    while i < bytes.len()
                        && (bytes[i].is_ascii_digit()
                            || bytes[i] == b'.'
                            || bytes[i] == b'e'
                            || bytes[i] == b'E'
                            || (bytes[i] == b'+' || bytes[i] == b'-')
                                && i > 0
                                && (bytes[i - 1] == b'e' || bytes[i - 1] == b'E'))
                    {
                        i += 1;
                    }
                    param_counter += 1;
                    out.push('$');
                    let _ = core::fmt::Write::write_fmt(
                        &mut out,
                        format_args!("{param_counter}"),
                    );
                    last_was_space = false;
                    continue;
                }
            }
            // Default: copy through byte-for-byte. Identifier-like
            // characters lower-case the alphabetic portion so
            // `SELECT * FROM T` and `select * from t` collapse.
            if b.is_ascii_uppercase() {
                out.push(b.to_ascii_lowercase() as char);
            } else {
                out.push(b as char);
            }
            last_was_space = false;
            i += 1;
        }
        // Trim trailing space.
        if out.ends_with(' ') {
            out.pop();
        }
        out
    }

    /// Record one execution. `elapsed_us` is the wall-clock micros
    /// between start and end; `now_us` is the wall-clock micros at
    /// completion (used for `last_seen_us`).
    ///
    /// v7.37.22 (22.6) — the sql key is normalised so distinct
    /// literal-bearing instances collapse to a single template.
    ///
    /// Row-count is reported via [`Self::record_with_rows`]; this
    /// shim defaults to 0 for callers that don't yet plumb the
    /// affected / produced row count through.
    pub fn record(&mut self, sql: &str, elapsed_us: u64, now_us: u64) {
        self.record_with_rows(sql, elapsed_us, now_us, 0);
    }

    /// v7.37.22 (22.9) — full record path with row count tracking.
    /// `rows` is the number of rows the executor produced
    /// (SELECT result.len()) or affected (INSERT/UPDATE/DELETE
    /// affected). Aggregates over the normalised template into
    /// `total_rows` (cumulative) and `max_rows` (per-call peak).
    pub fn record_with_rows(
        &mut self,
        sql: &str,
        elapsed_us: u64,
        now_us: u64,
        rows: u64,
    ) {
        let sql = &Self::normalize_sql(sql);
        let sql: &str = sql.as_str();
        if let Some(stat) = self.entries.get_mut(sql) {
            stat.exec_count = stat.exec_count.saturating_add(1);
            stat.total_us = stat.total_us.saturating_add(elapsed_us);
            stat.max_us = stat.max_us.max(elapsed_us);
            stat.last_seen_us = now_us;
            stat.total_rows = stat.total_rows.saturating_add(rows);
            stat.max_rows = stat.max_rows.max(rows);
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
                total_rows: rows,
                max_rows: rows,
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
    fn normalize_strips_numeric_literals() {
        assert_eq!(
            QueryStats::normalize_sql("SELECT * FROM t WHERE id = 1"),
            "select * from t where id = $1"
        );
        assert_eq!(
            QueryStats::normalize_sql("SELECT * FROM t WHERE id = 42 AND age > 30"),
            "select * from t where id = $1 and age > $2"
        );
    }

    #[test]
    fn normalize_strips_string_literals() {
        assert_eq!(
            QueryStats::normalize_sql("SELECT * FROM t WHERE name = 'alice'"),
            "select * from t where name = $1"
        );
        // Embedded escaped quote.
        assert_eq!(
            QueryStats::normalize_sql("SELECT 'a''b'"),
            "select $1"
        );
    }

    #[test]
    fn normalize_keeps_identifiers_with_digit_suffix() {
        assert_eq!(
            QueryStats::normalize_sql("SELECT col42 FROM t"),
            "select col42 from t"
        );
    }

    #[test]
    fn normalize_collapses_whitespace_and_strips_comments() {
        assert_eq!(
            QueryStats::normalize_sql(
                "  SELECT *\n  -- pick everything\n  FROM /* yes */ t  WHERE id = 1"
            ),
            "select * from t where id = $1"
        );
    }

    #[test]
    fn record_with_rows_tracks_total_and_max() {
        // v7.37.22 (22.9) — row count accumulates per template.
        let mut qs = QueryStats::new();
        qs.record_with_rows("SELECT * FROM t", 100, 1000, 5);
        qs.record_with_rows("SELECT * FROM t", 200, 2000, 12);
        qs.record_with_rows("SELECT * FROM t", 150, 3000, 3);
        let s = qs.get("SELECT * FROM t").expect("present");
        assert_eq!(s.exec_count, 3);
        assert_eq!(s.total_rows, 5 + 12 + 3);
        assert_eq!(s.max_rows, 12);
    }

    #[test]
    fn record_zero_default_keeps_row_counters_at_zero() {
        // The legacy `record(sql, elapsed, now)` shim should
        // leave row counters at 0 — preserves backwards
        // compatibility for callers that don't yet plumb rows.
        let mut qs = QueryStats::new();
        qs.record("SELECT 1", 100, 1000);
        let s = qs.get("SELECT 1").expect("present");
        assert_eq!(s.total_rows, 0);
        assert_eq!(s.max_rows, 0);
    }

    #[test]
    fn record_increments_counters() {
        // v7.37.22 (22.6) — `SELECT 1` normalises to `select $1`.
        // get() also normalises the lookup key. Two distinct
        // calls collapse to one entry per normalised template.
        let mut qs = QueryStats::new();
        qs.record("SELECT 1", 100, 1000);
        qs.record("SELECT 1", 200, 2000);
        let s = qs
            .entries
            .get("select $1")
            .expect("normalised template present");
        assert_eq!(s.exec_count, 2);
        assert_eq!(s.total_us, 300);
        assert_eq!(s.max_us, 200);
        assert_eq!(s.last_seen_us, 2000);
    }

    #[test]
    fn distinct_sql_yields_separate_entries() {
        // v7.37.22 (22.6) — only structurally different queries
        // create separate entries. Different literals collapse.
        let mut qs = QueryStats::new();
        qs.record("SELECT a FROM t WHERE id = 1", 10, 100);
        qs.record("SELECT a FROM t WHERE id = 2", 20, 200);
        // Both collapse to `select a from t where id = $1`.
        assert_eq!(qs.len(), 1);
        qs.record("SELECT b FROM t WHERE id = 1", 30, 300);
        // Now there's a structurally distinct template.
        assert_eq!(qs.len(), 2);
    }

    #[test]
    fn lru_evicts_oldest_at_cap() {
        // v7.37.22 (22.6) — fill the cap with structurally
        // distinct queries (different column names), then add
        // one more and verify the oldest evicts.
        let mut qs = QueryStats::new();
        for i in 0..QUERY_STATS_MAX {
            // Use distinct ident-shaped names so normalisation
            // doesn't collapse them.
            qs.record(&alloc::format!("SELECT c{i} FROM t"), 1, i as u64);
        }
        assert_eq!(qs.len(), QUERY_STATS_MAX);
        qs.record("SELECT new_col FROM t", 1, QUERY_STATS_MAX as u64);
        assert_eq!(qs.len(), QUERY_STATS_MAX);
        assert!(
            qs.entries.get("select c0 from t").is_none(),
            "oldest evicted"
        );
        assert!(qs.entries.get("select new_col from t").is_some());
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
