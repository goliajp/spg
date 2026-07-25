//! read01 round 427 (MySQL differential) — how an upsert counts its rows.
//!
//! Round 426 left this explicitly unmodelled because the sub-rules were only
//! partly measured. Measured exhaustively now (MariaDB 11), per row:
//!
//! | shape                                     | ON DUPLICATE | REPLACE |
//! |-------------------------------------------|--------------|---------|
//! | no conflict (plain insert)                | 1            | 1       |
//! | conflict, row CHANGED                     | 2            | 2       |
//! | conflict, row identical to what was there | 0            | 1       |
//!
//! The 2 is MySQL charging the update as delete+insert — but only when it
//! really changed something. `ON DUPLICATE` counts a no-op rewrite as 0;
//! `REPLACE` still charges 1 for its insert. Both are per row, so a mixed
//! statement adds up (1 changed-update + 1 insert = 3).
//!
//! SPG counted 1 per affected row everywhere, so `ROW_COUNT()` and the
//! statement's affected tag both under- or over-reported after an upsert.
//! REPLACE and ON DUPLICATE lower onto the SAME DO UPDATE clause; the
//! parser distinguishes them by the assignment list, which REPLACE leaves
//! empty (round 419), and that distinction now reaches the count.
//!
//! PostgreSQL counts one per affected row — unchanged.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn row_count(e: &mut Engine) -> i64 {
    match e.execute("SELECT ROW_COUNT()").unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::BigInt(n) => *n,
            Value::Int(n) => i64::from(*n),
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{sql}: {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = mysql();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT INTO t VALUES(1,10),(2,20)").unwrap();
    e
}

/// ON DUPLICATE: 1 when it inserted, 2 when it changed a row, 0 when the
/// rewrite was a no-op.
#[test]
fn on_duplicate_counts() {
    let mut e = seeded();
    // Conflict, identical value -> 0.
    e.execute("INSERT INTO t VALUES(1,10) ON DUPLICATE KEY UPDATE v = 10")
        .unwrap();
    assert_eq!(row_count(&mut e), 0);
    // Conflict, changed -> 2.
    e.execute("INSERT INTO t VALUES(1,11) ON DUPLICATE KEY UPDATE v = 11")
        .unwrap();
    assert_eq!(row_count(&mut e), 2);
    // No conflict -> 1.
    e.execute("INSERT INTO t VALUES(4,40) ON DUPLICATE KEY UPDATE v = 40")
        .unwrap();
    assert_eq!(row_count(&mut e), 1);
}

/// REPLACE: 1 when it inserted, 2 when it replaced different content, 1
/// when the existing row was already identical.
#[test]
fn replace_counts() {
    let mut e = seeded();
    // No existing row -> 1.
    e.execute("REPLACE INTO t VALUES(9,90)").unwrap();
    assert_eq!(row_count(&mut e), 1);
    // Existing, different -> 2.
    e.execute("REPLACE INTO t VALUES(9,91)").unwrap();
    assert_eq!(row_count(&mut e), 2);
    // Existing, identical -> 1 (REPLACE still charges for the insert).
    e.execute("REPLACE INTO t VALUES(9,91)").unwrap();
    assert_eq!(row_count(&mut e), 1);
}

/// The counts are PER ROW, so a mixed multi-row statement adds up.
#[test]
fn per_row_counts_add_up() {
    let mut e = seeded();
    // 1 changed-update (2) + 1 insert (1) = 3.
    e.execute("INSERT INTO t VALUES(1,11),(5,50) ON DUPLICATE KEY UPDATE v = VALUES(v)")
        .unwrap();
    assert_eq!(row_count(&mut e), 3);
    // Both rewrites are no-ops -> 0.
    e.execute("INSERT INTO t VALUES(1,11),(2,20) ON DUPLICATE KEY UPDATE v = VALUES(v)")
        .unwrap();
    assert_eq!(row_count(&mut e), 0);
    // REPLACE: 1 changed (2) + 1 new (1) = 3.
    e.execute("REPLACE INTO t VALUES(1,12),(7,70)").unwrap();
    assert_eq!(row_count(&mut e), 3);
    // REPLACE, both identical -> 1 + 1 = 2.
    e.execute("REPLACE INTO t VALUES(1,12),(7,70)").unwrap();
    assert_eq!(row_count(&mut e), 2);
}

/// INSERT IGNORE that skipped a row reports 0 (unchanged from before).
#[test]
fn insert_ignore_skipped_is_zero() {
    let mut e = seeded();
    e.execute("INSERT IGNORE INTO t VALUES(1,999)").unwrap();
    assert_eq!(row_count(&mut e), 0);
}

/// The same accounting drives the statement's own affected tag.
#[test]
fn affected_tag_matches() {
    let mut e = seeded();
    assert_eq!(
        affected(&mut e, "INSERT INTO t VALUES(1,10) ON DUPLICATE KEY UPDATE v = 10"),
        0
    );
    assert_eq!(
        affected(&mut e, "INSERT INTO t VALUES(1,11) ON DUPLICATE KEY UPDATE v = 11"),
        2
    );
    assert_eq!(affected(&mut e, "REPLACE INTO t VALUES(2,21)"), 2);
}

/// A PostgreSQL session counts ONE per affected row, changed or not, and
/// its ON CONFLICT values are unaffected.
#[test]
fn postgres_counts_one_per_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id INT PRIMARY KEY, v INT)").unwrap();
    e.execute("INSERT INTO t VALUES(1,10),(2,20)").unwrap();
    // A no-op DO UPDATE still counts 1 in PG.
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO t VALUES(1,10) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v"
        ),
        1
    );
    // A changed DO UPDATE also counts 1 (never 2).
    assert_eq!(
        affected(
            &mut e,
            "INSERT INTO t VALUES(1,99) ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v"
        ),
        1
    );
    match e.execute("SELECT v FROM t ORDER BY id").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].values[0], Value::Int(99));
        }
        other => panic!("{other:?}"),
    }
}
