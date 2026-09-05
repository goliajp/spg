//! v7.40.0 — `oid[]` is a column type, as it is on PostgreSQL 18.6.
//!
//! `DataType::OidArray` — its value, codec tag, wire encoding, and
//! every surface that names a type — has existed since v7.39 (round
//! 694). What was missing was the DDL spelling, so
//!
//! ```text
//!   CREATE TABLE t (c oid[])
//!     PostgreSQL 18.6   accepted, format_type reads `oid[]`
//!     SPG 7.39.13       ERROR: Oid[] not yet supported
//! ```
//!
//! Capability present, routing absent — one arm of the parser's
//! postfix-`[]` map, which is the same hand-kept list this repository
//! rewrote twice in 7.39.13. Measured on PostgreSQL 18.6 first:
//! twenty-four array spellings accepted there, eighteen reachable
//! here, and `oid[]` was the only one of the six whose storage type
//! already existed.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| {
            r.values
                .iter()
                .map(spg_engine::eval::value_to_text)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn an_oid_array_column_can_be_created_and_read_back() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, c oid[])").unwrap();
    e.execute("INSERT INTO t VALUES (1, '{1,2,3}'), (2, '{}'), (3, NULL)")
        .unwrap();
    assert_eq!(
        rows(&mut e, "SELECT id, c FROM t ORDER BY id"),
        ["1|{1,2,3}", "2|{}", "3|NULL"]
    );
}

/// The type must NAME itself the way PostgreSQL 18.6 does, on every
/// surface — the defect class 7.39.13 closed for `timetz` and `year`.
/// PostgreSQL reports `ARRAY` from `information_schema` for every
/// array and `oid[]` from `format_type`.
#[test]
fn an_oid_array_column_names_its_type() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id int, c oid[])").unwrap();
    e.execute("INSERT INTO t VALUES (1, '{7}')").unwrap();
    assert_eq!(rows(&mut e, "SELECT pg_typeof(c) FROM t"), ["oid[]"]);
    assert_eq!(
        rows(
            &mut e,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 't'::regclass AND attname = 'c'"
        ),
        ["oid[]"]
    );
    // PostgreSQL 18.6 reports `ARRAY` here for EVERY array column and
    // names the element type only through `format_type`; SPG already
    // matched that for the other eighteen, and `oid[]` joins them.
    assert_eq!(
        rows(
            &mut e,
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = 't' AND column_name = 'c'"
        ),
        ["ARRAY"]
    );
}

/// The arm was added to a match every other array spelling goes
/// through, so this is the fence: the eighteen that already worked
/// still do. `real[]`, `time[]`, `timetz[]`, `inet[]` and `xml[]` are
/// the five PostgreSQL 18.6 accepts that this version adds beside it.
#[test]
fn the_array_spellings_that_already_worked_still_do() {
    let mut e = Engine::new();
    for ty in [
        "text[]",
        "int[]",
        "bigint[]",
        "smallint[]",
        "bool[]",
        "double precision[]",
        "numeric[]",
        "date[]",
        "timestamp[]",
        "timestamptz[]",
        "uuid[]",
        "json[]",
        "jsonb[]",
        "bytea[]",
        "varchar(8)[]",
        "char(4)[]",
        "money[]",
        "interval[]",
        "oid[]",
    ] {
        let sql = format!("CREATE TABLE probe_arr (c {ty})");
        e.execute(&sql)
            .unwrap_or_else(|x| panic!("{ty} should be a column type: {x:?}"));
        e.execute("DROP TABLE probe_arr").unwrap();
    }
}
