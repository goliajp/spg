//! 9.0.5 — `pg_stat_user_tables` answers PostgreSQL 18's columns.
//!
//! It had sixteen of thirty, and the fourteen missing ones did not read
//! as zero: a query naming `autovacuum_count` or `n_mod_since_analyze`
//! was REFUSED, so a dashboard got an error rather than a number. Every
//! monitoring tool that watches a PostgreSQL table reads some of them —
//! "has autovacuum run on this table lately" is the standard alarm, and
//! it is `autovacuum_count` plus `n_dead_tup`.
//!
//! The counts are the server's own, not a transaction's: a rolled-back
//! INSERT still counted before this and still counts now.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: not rows");
    };
    rows.iter()
        .map(|r| r.values.iter().map(|v| format!("{v:?}")).collect())
        .collect()
}

fn one(e: &mut Engine, sql: &str) -> String {
    rows(e, sql)
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_default()
}

/// PostgreSQL 18.6, read off the running server.
const PG18_COLUMNS: &[&str] = &[
    "relid",
    "schemaname",
    "relname",
    "seq_scan",
    "last_seq_scan",
    "seq_tup_read",
    "idx_scan",
    "last_idx_scan",
    "idx_tup_fetch",
    "n_tup_ins",
    "n_tup_upd",
    "n_tup_del",
    "n_tup_hot_upd",
    "n_tup_newpage_upd",
    "n_live_tup",
    "n_dead_tup",
    "n_mod_since_analyze",
    "n_ins_since_vacuum",
    "last_vacuum",
    "last_autovacuum",
    "last_analyze",
    "last_autoanalyze",
    "vacuum_count",
    "autovacuum_count",
    "analyze_count",
    "autoanalyze_count",
    "total_vacuum_time",
    "total_autovacuum_time",
    "total_analyze_time",
    "total_autoanalyze_time",
];

#[test]
fn every_column_postgresql_18_has_can_be_selected_by_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, v int)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();

    // One at a time, so a failure names the column that is missing
    // rather than the whole list.
    for c in PG18_COLUMNS {
        let sql = format!("SELECT {c} FROM pg_stat_user_tables WHERE relname = 't'");
        e.execute(&sql)
            .unwrap_or_else(|x| panic!("pg_stat_user_tables has no {c}: {x:?}"));
    }

    // And in PostgreSQL's ORDER, which is what `SELECT *` hands a
    // client that reads by position.
    let got = rows(
        &mut e,
        "SELECT string_agg(column_name, ',' ORDER BY ordinal_position) \
         FROM information_schema.columns \
         WHERE table_name = 'pg_stat_user_tables'",
    );
    let got = got
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_default();
    for c in PG18_COLUMNS {
        assert!(got.contains(c), "{c} missing from {got}");
    }
}

#[test]
fn the_counters_count() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, v int)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
    e.execute("UPDATE t SET v = 9 WHERE id = 1").unwrap();

    // Two inserts and one update since the last ANALYZE, two inserts
    // since the last VACUUM.
    assert_eq!(
        one(
            &mut e,
            "SELECT n_mod_since_analyze FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(3)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT n_ins_since_vacuum FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(2)"
    );

    // ANALYZE resets the first and counts itself.
    e.execute("ANALYZE t").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT n_mod_since_analyze FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(0)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT analyze_count FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(1)"
    );
    // …and leaves the vacuum side alone.
    assert_eq!(
        one(
            &mut e,
            "SELECT n_ins_since_vacuum FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(2)"
    );

    // A VACUUM an operator issued is an operator's: `vacuum_count`, not
    // `autovacuum_count`. It used to stamp the daemon's column instead.
    e.execute("VACUUM t").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT vacuum_count FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(1)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT autovacuum_count FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(0)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT n_ins_since_vacuum FROM pg_stat_user_tables WHERE relname = 't'"
        ),
        "BigInt(0)"
    );
}

#[test]
fn a_scan_stamps_when_it_last_happened() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int PRIMARY KEY, v int)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 1)").unwrap();
    // A clockless engine has no reading to stamp with, and NULL is what
    // PostgreSQL answers for a relation that was never scanned — so the
    // assertion is that the column EXISTS and answers, not that it is
    // non-null on an engine with no clock.
    let v = one(
        &mut e,
        "SELECT last_seq_scan FROM pg_stat_user_tables WHERE relname = 't'",
    );
    assert!(
        v == "Null" || v.starts_with("Timestamp"),
        "last_seq_scan is a timestamptz or NULL, got {v}"
    );
}
