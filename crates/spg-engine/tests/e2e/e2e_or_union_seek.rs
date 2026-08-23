//! v7.38.19 — `a = 1 OR a = 2` seeks, the way `a IN (1, 2)` already did.
//!
//! Found while decomposing sentori's dashboard shape: with an ORDINARY
//! single-column index in place, so nothing else was in the way, on
//! 200,000 rows with the predicate matching nothing —
//!
//!     project_id IN (98, 99)               0.192 ms
//!     project_id = 99 OR project_id = 98   6.560 ms
//!
//! The two predicates mean the same thing. One took a descent and the
//! other read the table.
//!
//! The GIN path has unioned OR candidate sets since v7.17 and states
//! the rule this follows: emit a candidate set only if EVERY disjunct
//! seeks, because a disjunct that falls through to a scan contributes
//! rows the union would then be missing.

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
    e.execute("CREATE TABLE t (id int PRIMARY KEY, a int NOT NULL, b int NOT NULL)")
        .unwrap();
    for i in 0..300i32 {
        e.execute(&format!(
            "INSERT INTO t VALUES ({i}, {}, {})",
            i % 10,
            i % 6
        ))
        .unwrap();
    }
    e.execute("CREATE INDEX t_a ON t (a)").unwrap();
    e.execute("CREATE INDEX t_b ON t (b)").unwrap();
    e
}

fn seq_scans(e: &mut Engine) -> String {
    one(
        e,
        "SELECT seq_scan FROM pg_stat_user_tables WHERE relname = 't'",
    )
}

#[test]
fn an_or_of_two_equalities_seeks() {
    let mut e = seeded();
    let before = seq_scans(&mut e);
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 99 OR a = 98"),
        "BigInt(0)"
    );
    let after = seq_scans(&mut e);
    assert_eq!(before, after, "an OR of seekable disjuncts must not scan");
}

/// The rule the union rests on. `b * 2 = 4` is not seekable, so the
/// union would be missing its rows — the whole predicate has to fall
/// back to the scan, and the ANSWER is what proves it did.
#[test]
fn an_or_with_an_unseekable_disjunct_still_answers_correctly() {
    let mut e = seeded();
    // a = 0 matches 30 rows; b * 2 = 4 means b = 2, which matches 50;
    // rows with a = 0 AND b = 2 are counted once.
    let both = one(&mut e, "SELECT count(*) FROM t WHERE a = 0 AND b * 2 = 4");
    let or = one(&mut e, "SELECT count(*) FROM t WHERE a = 0 OR b * 2 = 4");
    assert_eq!(both, "BigInt(10)");
    assert_eq!(or, "BigInt(70)", "30 + 50 - 10");
}

/// A row matching BOTH disjuncts must be counted once. A union that
/// concatenated its arms would answer 80 here.
#[test]
fn a_row_matching_both_disjuncts_is_counted_once() {
    let mut e = seeded();
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 0"),
        "BigInt(30)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE b = 0"),
        "BigInt(50)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 0 AND b = 0"),
        "BigInt(10)"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE a = 0 OR b = 0"),
        "BigInt(70)",
        "30 + 50 - 10; a concatenated union answers 80"
    );
}

/// Disjuncts need not share a column or an operator — each arm goes
/// back through the same seek.
#[test]
fn the_arms_may_differ_in_column_and_in_operator() {
    let mut e = seeded();
    let expected = one(&mut e, "SELECT count(*) FROM t WHERE id > 295 OR a = 3");
    assert_eq!(
        expected, "BigInt(34)",
        "4 rows with id > 295, 30 with a = 3"
    );
    let before = seq_scans(&mut e);
    let _ = one(&mut e, "SELECT count(*) FROM t WHERE id > 295 OR a = 3");
    assert_eq!(
        before,
        seq_scans(&mut e),
        "a range OR an equality must seek"
    );
    // Three arms, nested as the parser leaves them.
    assert_eq!(
        one(
            &mut e,
            "SELECT count(*) FROM t WHERE a = 1 OR a = 2 OR a = 3"
        ),
        "BigInt(90)"
    );
}
