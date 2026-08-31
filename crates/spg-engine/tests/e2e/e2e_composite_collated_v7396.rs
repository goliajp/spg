//! v7.39.6 — a composite index over a TEXT leading column is usable on
//! a collated database, for the equality it was built for.
//!
//! The seek declined whenever the leading column collated, on the
//! grounds that the tree is keyed in a space the probe is not built in.
//! That is true of a single-column B-tree, whose entries are ICU sort
//! keys. It is not true of a composite one: `compose_multi_key` builds
//! every component with `IndexKey::from_value`, the raw cell — the same
//! reason `Table::index_collation` excludes composite indexes. Entries
//! and probe were always in the same space.
//!
//! And byte equality answers the predicate, because the collation is
//! deterministic: `'a' = 'A'`, `'a ' = 'a'` and `'é' = 'e'` are false
//! on PostgreSQL 18 and here alike.
//!
//! The published image collates, so on it every composite index over a
//! text column cost every write and bought no read. Measured, 200,000
//! rows, `WHERE a = 'zzz-nosuch' AND b = 42` over `(a, b)`: 7.88 ms
//! with the index against 7.53 without; the same table on a
//! byte-ordering server answered in 0.23 ms.
//!
//! These pins do not time anything. They ask for answers a probe in the
//! wrong space would get wrong — the row that exists, the near-misses
//! that must NOT come back, and the case and accent variants that
//! byte equality separates.

use spg_engine::{Engine, QueryResult};

/// Above the seek's own quarter-of-the-table bargain, so the index is
/// worth consulting rather than skipped as too broad.
const ROWS: i64 = 4000;

fn collated() -> Engine {
    let mut e = Engine::new();
    e.declare_database_collation("en_US.UTF-8")
        .expect("the test engine accepts a database collation");
    e.execute("CREATE TABLE c (id INT PRIMARY KEY, a TEXT, b INT)")
        .unwrap();
    e.execute(&format!(
        "INSERT INTO c SELECT g, 'k' || lpad(g::text, 8, '0'), g % 100 FROM generate_series(1, {ROWS}) g"
    ))
    .unwrap();
    // The rows whose spelling the collation could confuse.
    for (id, a, b) in [
        (900001, "Alpha", 7),
        (900002, "alpha", 7),
        (900003, "ALPHA", 7),
        (900004, "alpha ", 7),
        (900005, "éclair", 7),
        (900006, "eclair", 7),
    ] {
        e.execute(&format!("INSERT INTO c VALUES ({id}, '{a}', {b})"))
            .unwrap();
    }
    e.execute("CREATE INDEX cx ON c (a, b)").unwrap();
    e
}

fn ids(e: &mut Engine, sql: &str) -> Vec<i64> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows");
    };
    rows.iter()
        .map(|r| match &r.values[0] {
            spg_storage::Value::BigInt(n) => *n,
            spg_storage::Value::Int(n) => i64::from(*n),
            other => panic!("{sql}: {other:?}"),
        })
        .collect()
}

#[test]
fn the_matching_row_comes_back() {
    let mut e = collated();
    assert_eq!(
        ids(&mut e, "SELECT id FROM c WHERE a = 'k00000042' AND b = 42"),
        vec![42],
        "a probe landing in the wrong space returns nothing at all"
    );
}

#[test]
fn a_predicate_matching_nothing_returns_nothing() {
    let mut e = collated();
    assert!(ids(&mut e, "SELECT id FROM c WHERE a = 'zzz-nosuch' AND b = 42").is_empty());
}

#[test]
fn case_and_padding_and_accents_are_separate_values() {
    // Deterministic collation: equality is byte equality. If the seek
    // ever folded, these would fetch each other's rows.
    let mut e = collated();
    for (needle, want) in [
        ("Alpha", 900_001i64),
        ("alpha", 900_002),
        ("ALPHA", 900_003),
        ("alpha ", 900_004),
        ("éclair", 900_005),
        ("eclair", 900_006),
    ] {
        assert_eq!(
            ids(
                &mut e,
                &format!("SELECT id FROM c WHERE a = '{needle}' AND b = 7")
            ),
            vec![want],
            "a = '{needle}'"
        );
    }
}

#[test]
fn the_second_component_still_narrows() {
    // The composite's whole point: the same leading value with a `b`
    // that does not match must return nothing.
    let mut e = collated();
    assert!(
        ids(&mut e, "SELECT id FROM c WHERE a = 'alpha' AND b = 8").is_empty(),
        "the second component was ignored"
    );
}

#[test]
fn the_answers_match_a_table_with_no_index() {
    let mut e = collated();
    let mut plain = Engine::new();
    plain
        .declare_database_collation("en_US.UTF-8")
        .expect("collation");
    plain
        .execute("CREATE TABLE c (id INT PRIMARY KEY, a TEXT, b INT)")
        .unwrap();
    plain
        .execute(&format!(
            "INSERT INTO c SELECT g, 'k' || lpad(g::text, 8, '0'), g % 100 FROM generate_series(1, {ROWS}) g"
        ))
        .unwrap();
    for (id, a, b) in [
        (900001, "Alpha", 7),
        (900002, "alpha", 7),
        (900003, "ALPHA", 7),
        (900004, "alpha ", 7),
        (900005, "éclair", 7),
        (900006, "eclair", 7),
    ] {
        plain
            .execute(&format!("INSERT INTO c VALUES ({id}, '{a}', {b})"))
            .unwrap();
    }
    for sql in [
        "SELECT id FROM c WHERE a = 'k00000042' AND b = 42 ORDER BY id",
        "SELECT id FROM c WHERE a = 'alpha' AND b = 7 ORDER BY id",
        "SELECT id FROM c WHERE a = 'ALPHA' AND b = 7 ORDER BY id",
        "SELECT id FROM c WHERE a > 'k00003990' ORDER BY id",
        "SELECT id FROM c WHERE a = 'zzz' AND b = 1 ORDER BY id",
        "SELECT id FROM c ORDER BY a, id LIMIT 5",
    ] {
        assert_eq!(ids(&mut e, sql), ids(&mut plain, sql), "{sql}");
    }
}
