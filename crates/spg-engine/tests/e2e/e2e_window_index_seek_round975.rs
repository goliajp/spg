//! r975 — the window path asks the indices before it walks the table.
//!
//! Round 970 gave the single-table STREAMING walk an index step. The
//! window path is a different walk and had the same hole, and it is
//! reached by any statement carrying a window function — so a WHERE
//! naming an indexed column read the whole table. Measured on 400k rows:
//! `row_number() OVER () … WHERE id = 500`, a one-row answer on a
//! primary key, took 13.762 ms against PG18.4's 0.151, while the same
//! predicate without the window took 0.091. The cost did not depend on
//! how many rows survived (999 survivors: 13.312 ms) nor on row width
//! (13.312 narrow vs 13.327 wide) — which is what walking the whole
//! table looks like, and what a result-shaped cost does not.
//!
//! Witness is a COUNTER, not a clock: `pg_stat_user_tables` separates
//! `idx_scan` from `seq_scan`. Round 970 learned the other half of this
//! lesson — its first pin ran through an entry point the change did not
//! serve, so the answers were right and the counters never moved. Here
//! the window path IS what `Engine::execute` reaches, and the counters
//! are what proves it.

use spg_engine::Engine;

fn run(e: &mut Engine, sql: &str) {
    e.execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

/// `(idx_scan, seq_scan)` for `t`.
fn scan_counts(e: &mut Engine) -> (i64, i64) {
    let sql = "SELECT idx_scan, seq_scan FROM pg_stat_user_tables WHERE relname = 't'";
    let r = match e.execute(sql) {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => rows,
        other => panic!("{sql}: {other:?}"),
    };
    assert_eq!(r.len(), 1, "{sql} -> {r:?}");
    let cell = |i: usize| -> i64 {
        let v = format!("{:?}", r[0].values[i]);
        v.trim_start_matches(|c: char| !c.is_ascii_digit() && c != '-')
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("not a count: {v}"))
    };
    (cell(0), cell(1))
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    run(
        &mut e,
        "CREATE TABLE t (id INT PRIMARY KEY, k INT, g INT, s TEXT)",
    );
    run(
        &mut e,
        "INSERT INTO t SELECT g, (g*7)%40, g%4, CASE WHEN g%13 = 0 THEN NULL ELSE 'v'||g END \
         FROM generate_series(1,200) g",
    );
    run(&mut e, "CREATE INDEX t_k ON t (k)");
    run(&mut e, "DELETE FROM t WHERE id % 37 = 0");
    run(&mut e, "UPDATE t SET k = k + 500 WHERE id % 11 = 0");
    e
}

#[test]
fn a_window_query_with_a_pk_predicate_reaches_the_index() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let got = rows(
        &mut e,
        "SELECT id, row_number() OVER () FROM t WHERE id = 50",
    );
    assert_eq!(got, vec!["Int(50)|BigInt(1)".to_string()], "{got:?}");
    let (i1, s1) = scan_counts(&mut e);
    assert!(i1 > i0, "must reach an index: idx_scan {i0} -> {i1}");
    assert_eq!(s1, s0, "and must not walk the table: seq_scan {s0} -> {s1}");
}

#[test]
fn an_ordered_window_with_a_secondary_index_predicate_seeks_too() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let got = rows(
        &mut e,
        "SELECT id, rank() OVER (ORDER BY k) FROM t WHERE k = 7",
    );
    assert!(!got.is_empty(), "the fixture must have matches: {got:?}");
    let (i1, s1) = scan_counts(&mut e);
    assert!(
        i1 > i0,
        "a window's ORDER BY does not stop the seek: idx_scan {i0} -> {i1}"
    );
    assert_eq!(s1, s0, "seq_scan {s0} -> {s1}");
}

#[test]
fn a_window_query_with_no_indexable_predicate_still_walks_the_table() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let _ = rows(
        &mut e,
        "SELECT id, row_number() OVER () FROM t WHERE s LIKE '%9%'",
    );
    let (i1, s1) = scan_counts(&mut e);
    assert_eq!(i1, i0, "nothing to seek on: idx_scan {i0} -> {i1}");
    assert!(s1 > s0, "so it scans: seq_scan {s0} -> {s1}");
}

#[test]
fn a_window_over_the_whole_table_still_walks_it() {
    let mut e = seeded();
    let (i0, s0) = scan_counts(&mut e);
    let got = rows(&mut e, "SELECT id, row_number() OVER () FROM t");
    assert!(got.len() > 100, "every row: {}", got.len());
    let (i1, s1) = scan_counts(&mut e);
    assert_eq!(i1, i0, "no WHERE, nothing to seek: idx_scan {i0} -> {i1}");
    assert!(s1 > s0, "seq_scan {s0} -> {s1}");
}

#[test]
fn the_seek_only_narrows_the_full_where_still_runs() {
    let mut e = seeded();
    // `id = 50` hits the index, `s IS NULL` does not, and 50 is not a
    // multiple of 13 — so the row must be filtered out. A seek that
    // skipped the WHERE would answer one row.
    let got = rows(
        &mut e,
        "SELECT id, row_number() OVER () FROM t WHERE id = 50 AND s IS NULL",
    );
    assert!(got.is_empty(), "{got:?}");
    let got = rows(
        &mut e,
        "SELECT id, row_number() OVER () FROM t WHERE id = 52 AND s IS NULL",
    );
    assert_eq!(got, vec!["Int(52)|BigInt(1)".to_string()], "{got:?}");
}

#[test]
fn the_window_still_sees_exactly_the_rows_it_should() {
    let mut e = seeded();
    // A deleted row stays deleted, and an updated one has a single live
    // version — the seek's candidates go through the same visibility
    // predicate the scan applies.
    assert!(
        rows(
            &mut e,
            "SELECT id, row_number() OVER () FROM t WHERE id = 37"
        )
        .is_empty(),
        "deleted"
    );
    let got = rows(
        &mut e,
        "SELECT id, row_number() OVER () FROM t WHERE id = 11",
    );
    assert_eq!(got.len(), 1, "one live version: {got:?}");

    // And the numbering over a seek's rows is the numbering over the
    // scan's: `OFFSET 0` does not change what the window sees, so the two
    // must agree as sets for every partition.
    for pred in ["id BETWEEN 40 AND 80", "k = 7", "k BETWEEN 5 AND 15"] {
        let mut a = rows(
            &mut e,
            &format!("SELECT id, rank() OVER (PARTITION BY g ORDER BY k) FROM t WHERE {pred}"),
        );
        let mut b = rows(
            &mut e,
            &format!(
                "SELECT id, rank() OVER (PARTITION BY g ORDER BY k) FROM t WHERE {pred} OFFSET 0"
            ),
        );
        a.sort();
        b.sort();
        assert_eq!(a, b, "WHERE {pred}");
    }
}
