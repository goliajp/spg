// pedantic doc_markdown flags the embedded wire-format spec block
// and a handful of proper nouns; allowing at the module level
// keeps the spec readable.
#![allow(clippy::doc_markdown)]

//! v6.2.0 — per-column statistics for the cost-based optimizer.
//!
//! Each analysed `(table, column)` pair gets a [`ColumnStats`]
//! row: `null_frac` ∈ [0.0, 1.0], `n_distinct` count, and a
//! 100-bucket equi-depth histogram (`Vec<String>` of 101 bounds —
//! v0 .. v100). Skewed distributions live in the bucket widths,
//! not in a separate MCV sidecar (see `V6_2_DESIGN.md` deliberation
//! #1).
//!
//! Storage shape mirrors [`crate::publications::Publications`] and
//! [`crate::subscriptions::Subscriptions`]:
//!   - `BTreeMap<(String, String), ColumnStats>` keeps iteration
//!     in deterministic alphabetical order (snapshot byte-stable
//!     regardless of insertion sequence).
//!   - `BTreeMap<String, u64>` tracks per-table modified-row count
//!     for v6.2.1 auto-analyze's 10 % threshold trigger.
//!
//! Persistence rides the snapshot envelope's v5 trailer block (see
//! `crate::lib::build_envelope`). v1/v2/v3/v4 envelopes deserialise
//! to empty statistics; v5 writers always emit the trailer.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Per-column statistics computed by ANALYZE. See module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    /// Fraction of NULL rows. `[0.0, 1.0]`.
    pub null_frac: f32,
    /// Raw distinct-value count (linear-counting estimate; PG
    /// stores a similar approximation. v6.2.x can re-tune the
    /// sketch — the value semantic is "approximate ≥ 0.95
    /// accuracy on skewed corpora").
    pub n_distinct: u64,
    /// 101 sorted bounds → 100 equi-depth buckets. Bucket `i`
    /// spans values in `[bounds[i], bounds[i+1])` for i < 99,
    /// and `[bounds[99], bounds[100]]` for the final bucket.
    /// Bounds are the canonical SQL textual form of the column's
    /// values — TEXT lexicographic, INT decimal, FLOAT
    /// shortest-round-trip, DATE/TIMESTAMP ISO. Empty for an
    /// all-NULL column.
    pub histogram_bounds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Statistics {
    /// Keyed on `(table_name, column_name)`. BTreeMap orders by
    /// `(table, column)` so iteration is deterministic for
    /// snapshot byte-stability and SHOW-style introspection.
    inner: BTreeMap<(String, String), ColumnStats>,
    /// Per-table modified-row counter since the last ANALYZE on
    /// that table. v6.2.1 auto-analyze fires when this fraction
    /// crosses 10 % of the live row count.
    modified_since: BTreeMap<String, u64>,
    /// v6.3.1 — monotonic version bumped on every successful
    /// ANALYZE. The plan cache snapshots this at prepare time;
    /// cache lookup compares and evicts on mismatch.
    ///
    /// In-memory only. Does NOT ride the envelope (plan cache is
    /// in-memory only too, so version starts at 0 on every Engine
    /// boot).
    version: u64,
}

// Statistics holds f32 (null_frac) so it can't auto-derive `Eq`.
// ANALYZE never stores NaN, so PartialEq is total in practice; we
// just don't claim Eq.

#[derive(Debug, PartialEq, Eq)]
pub enum StatisticsError {
    Corrupt(String),
}

impl Statistics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn get(&self, table: &str, column: &str) -> Option<&ColumnStats> {
        self.inner.get(&(table.to_string(), column.to_string()))
    }

    /// Iterate `((table, column), stats)` in deterministic
    /// alphabetical order. Used by `SELECT * FROM spg_statistic`
    /// and by snapshot serialisation.
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &ColumnStats)> {
        self.inner.iter()
    }

    /// Replace (or insert) the stats for one `(table, column)`.
    /// Called by ANALYZE per column.
    pub fn set(&mut self, table: String, column: String, stats: ColumnStats) {
        self.inner.insert((table, column), stats);
    }

    /// Drop every row whose table key matches `table`. Called
    /// before re-ANALYZE on a table so columns dropped between
    /// analyses don't leave stale rows behind.
    pub fn clear_table(&mut self, table: &str) {
        self.inner.retain(|(t, _), _| t != table);
    }

    /// Reset the modified-row counter for `table`. Called at the
    /// end of ANALYZE so v6.2.1 auto-analyze starts a fresh
    /// window.
    pub fn reset_modified(&mut self, table: &str) {
        self.modified_since.insert(table.to_string(), 0);
    }

    /// Bump the modified-row counter. The engine's
    /// `exec_insert` / `exec_update` / `exec_delete` paths feed
    /// this hook so v6.2.1 auto-analyze can read it.
    pub fn record_modifications(&mut self, table: &str, n: u64) {
        let entry = self.modified_since.entry(table.to_string()).or_default();
        *entry = entry.saturating_add(n);
    }

    pub fn modified_since_last_analyze(&self, table: &str) -> u64 {
        self.modified_since.get(table).copied().unwrap_or(0)
    }

    /// v6.3.1 — current monotonic version. Plan cache snapshots this
    /// at prepare time; lookup compares and evicts on mismatch.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// v6.3.1 — bumps the version. Called by `exec_analyze` after a
    /// successful ANALYZE on any table.
    pub fn bump_version(&mut self) {
        self.version = self.version.saturating_add(1);
    }

    // ── serialisation (envelope v5 trailer) ─────────────────────

    /// Format (each block little-endian):
    ///   [u16 num_columns]
    ///   for each column:
    ///     [u16 table_len][table bytes]
    ///     [u16 col_len][col bytes]
    ///     [f32 null_frac]
    ///     [u64 n_distinct]
    ///     [u16 num_bounds]
    ///     for each bound: [u16 b_len][b bytes]
    ///   [u16 num_modified_entries]
    ///   for each: [u16 t_len][t bytes][u64 modified_count]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.inner.len() * 32);
        let n = u16::try_from(self.inner.len()).expect("≤ 65,535 column-stats rows");
        out.extend_from_slice(&n.to_le_bytes());
        for ((table, col), stats) in &self.inner {
            write_str(&mut out, table);
            write_str(&mut out, col);
            out.extend_from_slice(&stats.null_frac.to_le_bytes());
            out.extend_from_slice(&stats.n_distinct.to_le_bytes());
            let nb =
                u16::try_from(stats.histogram_bounds.len()).expect("≤ 65,535 histogram bounds");
            out.extend_from_slice(&nb.to_le_bytes());
            for b in &stats.histogram_bounds {
                write_str(&mut out, b);
            }
        }
        let m =
            u16::try_from(self.modified_since.len()).expect("≤ 65,535 modified-row counters");
        out.extend_from_slice(&m.to_le_bytes());
        for (table, count) in &self.modified_since {
            write_str(&mut out, table);
            out.extend_from_slice(&count.to_le_bytes());
        }
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Self, StatisticsError> {
        let mut p = 0usize;
        let n = read_u16(buf, &mut p)? as usize;
        let mut inner = BTreeMap::new();
        for _ in 0..n {
            let table = read_str(buf, &mut p)?;
            let col = read_str(buf, &mut p)?;
            let null_frac_bytes = read_bytes(buf, &mut p, 4)?;
            let null_frac =
                f32::from_le_bytes(null_frac_bytes.try_into().map_err(|_| {
                    StatisticsError::Corrupt("null_frac slice".to_string())
                })?);
            let n_distinct = read_u64(buf, &mut p)?;
            let nb = read_u16(buf, &mut p)? as usize;
            let mut bounds = Vec::with_capacity(nb);
            for _ in 0..nb {
                bounds.push(read_str(buf, &mut p)?);
            }
            if inner
                .insert(
                    (table.clone(), col.clone()),
                    ColumnStats {
                        null_frac,
                        n_distinct,
                        histogram_bounds: bounds,
                    },
                )
                .is_some()
            {
                return Err(StatisticsError::Corrupt(alloc::format!(
                    "duplicate spg_statistic key ({table:?}, {col:?})"
                )));
            }
        }
        let m = read_u16(buf, &mut p)? as usize;
        let mut modified_since = BTreeMap::new();
        for _ in 0..m {
            let table = read_str(buf, &mut p)?;
            let count = read_u64(buf, &mut p)?;
            modified_since.insert(table, count);
        }
        if p != buf.len() {
            return Err(StatisticsError::Corrupt(alloc::format!(
                "trailing bytes in statistics payload: read {p}, len {}",
                buf.len()
            )));
        }
        Ok(Self {
            inner,
            modified_since,
            version: 0,
        })
    }
}

// ── histogram builder ───────────────────────────────────────────

/// v6.2.0 — 100-bucket equi-depth histogram bound count (101
/// boundary values). v6.2.x can re-tune.
pub const NUM_BUCKETS: usize = 100;

/// Build an equi-depth histogram over a (sorted) sample of textual
/// column values. Returns the 101 boundary strings, or an empty
/// vec when the input has no non-NULL values.
///
/// The caller sorts the sample via the column's natural ordering
/// (TEXT lexicographic, INT decimal, etc.) and hands us the
/// already-stringified values — we don't try to reason about types
/// here. Equi-depth means each consecutive pair of bounds spans
/// approximately `sample.len() / NUM_BUCKETS` values; selectivity
/// estimation in v6.2.2 walks bounds directly.
pub fn build_histogram(sorted_values: &[String]) -> Vec<String> {
    if sorted_values.is_empty() {
        return Vec::new();
    }
    let n = sorted_values.len();
    // 101 bounds = 100 buckets. With fewer than 101 values, every
    // value becomes its own bound (smaller histograms degrade
    // gracefully — selectivity still works).
    if n <= NUM_BUCKETS + 1 {
        return sorted_values.to_vec();
    }
    let mut bounds = Vec::with_capacity(NUM_BUCKETS + 1);
    for i in 0..=NUM_BUCKETS {
        // `i / NUM_BUCKETS` ∈ [0, 1] → index ∈ [0, n-1]
        let idx = (i as u64 * (n as u64 - 1)) / NUM_BUCKETS as u64;
        bounds.push(sorted_values[idx as usize].clone());
    }
    bounds
}

/// v6.2.0 — n_distinct estimator. Linear-counting sketch over the
/// already-sorted-and-deduped sample. Returns the exact distinct
/// count on a complete sample; on a reservoir sample, returns the
/// observed count (which v6.2.x can swap for HyperLogLog if needed).
pub fn estimate_n_distinct(sorted_values: &[String]) -> u64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let mut count: u64 = 1;
    let mut prev = &sorted_values[0];
    for v in &sorted_values[1..] {
        if v != prev {
            count += 1;
            prev = v;
        }
    }
    count
}

// ── byte-codec helpers ──────────────────────────────────────────

fn write_str(out: &mut Vec<u8>, s: &str) {
    let n = u16::try_from(s.len())
        .expect("table / column / bound names ≤ 65,535 bytes");
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn read_bytes<'a>(
    buf: &'a [u8],
    p: &mut usize,
    n: usize,
) -> Result<&'a [u8], StatisticsError> {
    let slice = buf.get(*p..*p + n).ok_or_else(|| {
        StatisticsError::Corrupt(alloc::format!("short read ({n} bytes)"))
    })?;
    *p += n;
    Ok(slice)
}

fn read_u16(buf: &[u8], p: &mut usize) -> Result<u16, StatisticsError> {
    let bytes = read_bytes(buf, p, 2)?;
    Ok(u16::from_le_bytes(bytes.try_into().map_err(|_| {
        StatisticsError::Corrupt("u16 slice".to_string())
    })?))
}

fn read_u64(buf: &[u8], p: &mut usize) -> Result<u64, StatisticsError> {
    let bytes = read_bytes(buf, p, 8)?;
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
        StatisticsError::Corrupt("u64 slice".to_string())
    })?))
}

fn read_str(buf: &[u8], p: &mut usize) -> Result<String, StatisticsError> {
    let n = read_u16(buf, p)? as usize;
    let slice = read_bytes(buf, p, n)?;
    core::str::from_utf8(slice)
        .map(ToString::to_string)
        .map_err(|e| StatisticsError::Corrupt(alloc::format!("non-UTF-8 str: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_cs(null_frac: f32, n_distinct: u64, bounds: &[&str]) -> ColumnStats {
        ColumnStats {
            null_frac,
            n_distinct,
            histogram_bounds: bounds.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_roundtrips() {
        let s = Statistics::new();
        let bytes = s.serialize();
        let s2 = Statistics::deserialize(&bytes).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn single_column_roundtrips() {
        let mut s = Statistics::new();
        s.set(
            "users".into(),
            "id".into(),
            mk_cs(0.0, 1000, &["1", "500", "1000"]),
        );
        let s2 = Statistics::deserialize(&s.serialize()).unwrap();
        assert_eq!(s, s2);
        let got = s2.get("users", "id").unwrap();
        assert_eq!(got.n_distinct, 1000);
        assert_eq!(got.histogram_bounds.len(), 3);
    }

    #[test]
    fn multi_column_roundtrips_with_modified_counter() {
        let mut s = Statistics::new();
        s.set("users".into(), "id".into(), mk_cs(0.0, 100, &["1", "50", "100"]));
        s.set(
            "users".into(),
            "name".into(),
            mk_cs(0.1, 99, &["alice", "bob", "zoe"]),
        );
        s.record_modifications("users", 17);
        let s2 = Statistics::deserialize(&s.serialize()).unwrap();
        assert_eq!(s, s2);
        assert_eq!(s2.modified_since_last_analyze("users"), 17);
    }

    #[test]
    fn histogram_bounds_count_is_101_for_100_buckets() {
        // 1000 sorted values → equi-depth 100 buckets → 101 bounds.
        let vals: Vec<String> = (0..1000).map(|i| alloc::format!("{i:04}")).collect();
        let bounds = build_histogram(&vals);
        assert_eq!(bounds.len(), 101);
        // First + last bound match min + max.
        assert_eq!(bounds.first().unwrap(), "0000");
        assert_eq!(bounds.last().unwrap(), "0999");
    }

    #[test]
    fn deterministic_serialise_independent_of_insert_order() {
        let mut s1 = Statistics::new();
        s1.set("z".into(), "c1".into(), mk_cs(0.0, 1, &["x"]));
        s1.set("a".into(), "c2".into(), mk_cs(0.0, 1, &["y"]));
        let mut s2 = Statistics::new();
        s2.set("a".into(), "c2".into(), mk_cs(0.0, 1, &["y"]));
        s2.set("z".into(), "c1".into(), mk_cs(0.0, 1, &["x"]));
        assert_eq!(s1.serialize(), s2.serialize());
    }

    #[test]
    fn n_distinct_estimator_within_5pct_on_uniform_corpus() {
        // 10000 values with exactly 100 distinct values, repeated.
        let mut vals: Vec<String> = Vec::with_capacity(10000);
        for i in 0..10000 {
            vals.push(alloc::format!("v{}", i % 100));
        }
        vals.sort();
        let est = estimate_n_distinct(&vals);
        // Linear-counting on a sorted complete sample returns the
        // exact count, so this is 100.
        assert_eq!(est, 100);
    }

    #[test]
    fn clear_table_drops_only_target_rows() {
        let mut s = Statistics::new();
        s.set("a".into(), "c1".into(), mk_cs(0.0, 1, &["x"]));
        s.set("a".into(), "c2".into(), mk_cs(0.0, 1, &["y"]));
        s.set("b".into(), "c1".into(), mk_cs(0.0, 1, &["z"]));
        s.clear_table("a");
        assert_eq!(s.len(), 1);
        assert!(s.get("a", "c1").is_none());
        assert!(s.get("b", "c1").is_some());
    }

    #[test]
    fn corrupt_short_read_errors() {
        // num_columns = 1 but no payload.
        let buf = 1u16.to_le_bytes();
        let err = Statistics::deserialize(&buf).unwrap_err();
        assert!(matches!(err, StatisticsError::Corrupt(_)));
    }

    #[test]
    fn build_histogram_passthrough_when_sample_is_small() {
        let vals: Vec<String> = (0..5).map(|i| alloc::format!("v{i}")).collect();
        let bounds = build_histogram(&vals);
        // <= 101 distinct values → every value is a bound.
        assert_eq!(bounds.len(), 5);
        assert_eq!(bounds[0], "v0");
        assert_eq!(bounds[4], "v4");
    }
}
