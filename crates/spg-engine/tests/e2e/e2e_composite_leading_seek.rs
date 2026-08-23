//! v7.38.19 — a composite index must answer a question about its
//! LEADING column alone.
//!
//! Found on sentori's own dashboard shape, decomposed stage by stage
//! against PostgreSQL 18 on a quiet machine. Their `events` table
//! carries `(project_id, kind)`, and their dashboard filters on
//! `project_id` alone:
//!
//!   predicate                          rows      SPG     PG18
//!   project_id = 3                    25000    3.901    0.777
//!   project_id = 3 AND kind='click'       0    0.192    0.213
//!   project_id = 99                       0    3.717    0.218   <-- 17x
//!
//! The third row is the one that names the defect. A predicate matching
//! NOTHING cost as much as one matching a quarter of the table, which
//! is what a full scan costs and what an index lookup never does. Give
//! the same table a single-column index on `project_id` and it went
//! 3.402 -> 0.188 ms; drop that index again and it went back to 2.994.
//!
//! The cause: `Table::index_on` returns only `IndexKind::BTree`, and a
//! two-column index is `BTreeMulti`. The bare-equality path therefore
//! saw no index at all. The prefix walk it needed already existed and
//! was already correct -- but it lived inside the `AND` branch, so it
//! was reachable only from a predicate with a second conjunct.
//!
//! The assertion is on `seq_scan`, not on elapsed time: this defect IS
//! "the table was scanned", and that is counted rather than sampled.
//! A timing bound tight enough to catch it would flake on a shared box.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(t) => t.to_string(),
            spg_storage::Value::Null => "<NULL>".into(),
            other => format!("{other:?}"),
        },
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ev (id int PRIMARY KEY, project_id int NOT NULL, kind text NOT NULL)")
        .unwrap();
    for i in 0..400i32 {
        e.execute(&format!(
            "INSERT INTO ev VALUES ({i}, {}, 'k{}')",
            i % 8,
            i % 4
        ))
        .unwrap();
    }
    // The ONLY index on `project_id` is a composite whose second
    // component the predicate will not mention. That is sentori's
    // shape, and it is the whole test.
    e.execute("CREATE INDEX ev_pk ON ev (project_id, kind)")
        .unwrap();
    e
}

fn seq_scans(e: &mut Engine) -> String {
    one(
        e,
        "SELECT seq_scan FROM pg_stat_user_tables WHERE relname = 'ev'",
    )
}

#[test]
fn a_leading_column_equality_seeks_a_composite_index() {
    let mut e = seeded();
    let before = seq_scans(&mut e);
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id = 3"),
        "BigInt(50)"
    );
    let after = seq_scans(&mut e);
    assert_eq!(
        before, after,
        "a leading-column equality must seek the composite index, not scan the table"
    );
}

#[test]
fn a_leading_column_equality_that_matches_nothing_seeks_too() {
    // The sharper form. A scan cannot tell the difference between this
    // and the previous query; a seek answers it without reading a row.
    let mut e = seeded();
    let before = seq_scans(&mut e);
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id = 99"),
        "BigInt(0)"
    );
    let after = seq_scans(&mut e);
    assert_eq!(before, after, "a miss must cost a descent, not a scan");
}

#[test]
fn the_rows_are_still_right() {
    // A seek that returns the leading column's whole prefix and forgets
    // to re-check anything else would answer these wrongly. The
    // candidate set is over-approximate by design; the caller re-applies
    // the predicate.
    let mut e = seeded();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id = 3"),
        "BigInt(50)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ev WHERE project_id = 3 AND kind = 'k3'"
        ),
        "BigInt(50)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ev WHERE project_id = 3 AND kind = 'k0'"
        ),
        "BigInt(0)"
    );
    assert_eq!(
        one(&mut e, "SELECT min(id) FROM ev WHERE project_id = 5"),
        "Int(5)"
    );
}

/// The rest of the class. Every one of these is seekable the moment a
/// single-column index exists, which is what says the predicate FORM
/// was never the obstacle -- the composite index was simply invisible
/// to all of them. Measured on 200k rows, each predicate matching
/// nothing, before any of this:
///
///   project_id = 99                    3.723 ms   PG 0.203   18x
///   project_id IN (98, 99)             3.578      PG 0.215   17x
///   project_id > 90                    4.067      PG 0.220   18x
///   project_id BETWEEN 90 AND 99       7.290      PG 0.203   36x
#[test]
fn the_other_leading_column_predicate_forms_seek_too() {
    for pred in [
        "project_id IN (98, 99)",
        "project_id > 90",
        "project_id BETWEEN 90 AND 99",
        "project_id >= 99",
        "project_id < 0",
    ] {
        let mut e = seeded();
        let before = seq_scans(&mut e);
        assert_eq!(
            one(&mut e, &format!("SELECT count(*) FROM ev WHERE {pred}")),
            "BigInt(0)",
            "{pred} matches nothing in this fixture"
        );
        let after = seq_scans(&mut e);
        assert_eq!(before, after, "{pred} must seek the composite index");
    }
}

/// And the bounds are the bounds. A leading-column range walk that got
/// the exclusive end wrong would answer these off by one group of 50.
#[test]
fn the_leading_range_bounds_are_exact() {
    let mut e = seeded();
    // project_id is i % 8, fifty rows each.
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id > 5"),
        "BigInt(100)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id >= 5"),
        "BigInt(150)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id < 2"),
        "BigInt(100)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id <= 2"),
        "BigInt(150)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ev WHERE project_id BETWEEN 2 AND 4"
        ),
        "BigInt(150)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM ev WHERE project_id > 2 AND project_id < 5"
        ),
        "BigInt(100)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM ev WHERE project_id IN (1, 7)"),
        "BigInt(100)"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT sum(project_id) FROM ev WHERE project_id BETWEEN 6 AND 7" /* PG: sum(int) is bigint */
        ),
        "BigInt(650)"
    );
}
