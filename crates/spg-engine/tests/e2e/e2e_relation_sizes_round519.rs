//! v7.39 (round 519) — what a relation weighs.
//!
//! The constant-answer probe reported `pg_database_size`,
//! `pg_total_relation_size` and `pg_indexes_size` as stubs. They were not.
//! It had asked them about `pg_class` — a catalog relation SPG SYNTHESISES,
//! so it has no bytes of its own — and about a database with no tables in
//! it, which really does weigh nothing. The functions have been wired to
//! the engine's own accounting all along: `Table::hot_bytes` for the heap
//! and `IndexKind::approx_resident_bytes` per index, the same meters
//! `spg_admin` reports.
//!
//! What WAS wrong was the two ends, and they were inverted against PG:
//!
//!   an oid naming nothing   PG: NULL     SPG: 0
//!   a relation with no rows PG: 0        SPG: NULL
//!
//! A monitoring query summing sizes therefore skipped the rows it should
//! have counted as zero and counted the ones it should have skipped.
//!
//! The byte COUNTS are SPG's own and do not match PG's — different storage,
//! and PG's figure is pages including overhead. What has to match is the
//! contract every tool actually tests, and that is what is pinned.
//!
//! Every expectation below is a PG18 reading.

use spg_engine::{Engine, QueryResult};

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sz (a INT, b TEXT)").unwrap();
    for i in 0..200 {
        e.execute(&format!("INSERT INTO sz VALUES ({i}, '{}')", "x".repeat(100)))
            .unwrap();
    }
    e
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn num(e: &mut Engine, sql: &str) -> i64 {
    text(e, sql).parse().unwrap_or_else(|_| panic!("{sql} is not a number"))
}

/// The contract a monitoring query depends on.
#[test]
fn round519_a_stored_relation_weighs_something() {
    let mut e = engine();
    assert!(num(&mut e, "SELECT pg_relation_size('sz')") > 0);
    // No index yet.
    assert_eq!(num(&mut e, "SELECT pg_indexes_size('sz')"), 0);
    assert_eq!(
        num(&mut e, "SELECT pg_total_relation_size('sz')"),
        num(&mut e, "SELECT pg_relation_size('sz')")
    );

    e.execute("CREATE INDEX sz_a ON sz(a)").unwrap();
    assert!(num(&mut e, "SELECT pg_indexes_size('sz')") > 0);
    assert!(
        num(&mut e, "SELECT pg_total_relation_size('sz')")
            > num(&mut e, "SELECT pg_relation_size('sz')")
    );
    // A database holding that table is not empty.
    assert!(num(&mut e, "SELECT pg_database_size(current_database())") > 0);
}

/// The two ends, which were the other way round.
#[test]
fn round519_nothing_is_null_and_empty_is_zero() {
    let mut e = engine();
    // An oid that names nothing: NULL, not 0.
    for sql in [
        "SELECT pg_relation_size(999999::oid)",
        "SELECT pg_total_relation_size(999999::oid)",
        "SELECT pg_indexes_size(999999::oid)",
    ] {
        assert_eq!(text(&mut e, sql), "NULL", "{sql}");
    }
    // A name that is not a relation at all: NULL.
    assert_eq!(text(&mut e, "SELECT pg_relation_size('nosuchtable')"), "NULL");

    // A relation that exists and stores nothing: 0, not NULL.
    e.execute("CREATE VIEW vsz AS SELECT 1 AS a").unwrap();
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_relation_size('vsz'), pg_total_relation_size('vsz'), pg_indexes_size('vsz')"
        ),
        "0|0|0"
    );
    // A synthesised catalog relation is the same case: it exists, and SPG
    // stores none of it.
    assert_eq!(text(&mut e, "SELECT pg_total_relation_size('pg_class')"), "0");
}

/// The bytes are real enough to move when the data does.
#[test]
fn round519_the_size_tracks_the_rows() {
    let mut e = engine();
    let before = num(&mut e, "SELECT pg_relation_size('sz')");
    for i in 200..400 {
        e.execute(&format!("INSERT INTO sz VALUES ({i}, '{}')", "y".repeat(100)))
            .unwrap();
    }
    let after = num(&mut e, "SELECT pg_relation_size('sz')");
    assert!(after > before, "{before} -> {after}");
}
