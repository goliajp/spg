//! 9.0.0 — `format_type` can name every type SPG's own `pg_type` lists.
//!
//! Swept oid by oid against PostgreSQL 18.6: of the 97 rows in SPG's
//! `pg_type` below oid 10000, 22 rendered as `???` — what `format_type`
//! answers for an oid it does not recognise. A catalog that lists a type
//! and a renderer that cannot name it are one catalog disagreeing with
//! itself, and psql showed it: `SELECT '(1,2)'::point \gdesc` said `???`.
//!
//! The first test needs no oracle: it asks the engine to agree with
//! itself over every row, so a type added to `pg_type` later without a
//! name fails here. The second pins a handful against PG's own answers.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn texts(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| match r.values.into_iter().next().expect("one column") {
            Value::Text(s) => s.to_string(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn every_pg_type_row_has_a_name() {
    let mut e = Engine::new();
    let unnamed = texts(
        &mut e,
        "SELECT oid::text || ' ' || typname FROM pg_type \
         WHERE oid < 10000 AND format_type(oid, -1) = '???' ORDER BY oid",
    );
    // The sweep would be vacuous over an empty catalog.
    let total = texts(
        &mut e,
        "SELECT count(*)::text FROM pg_type WHERE oid < 10000",
    );
    assert!(
        total[0].parse::<usize>().expect("a count") >= 90,
        "pg_type has {} rows below 10000",
        total[0]
    );
    assert!(unnamed.is_empty(), "format_type cannot name: {unnamed:?}");
}

#[test]
fn format_type_answers_what_pg_answers() {
    let mut e = Engine::new();
    for (oid, want) in [
        (24, "regproc"),
        (600, "point"),
        (603, "box"),
        (1008, "regproc[]"),
        (1034, "aclitem[]"),
        (2205, "regclass"),
        (2206, "regtype"),
        (2278, "void"),
        (3220, "pg_lsn"),
        (4451, "int4multirange"),
    ] {
        assert_eq!(
            texts(&mut e, &format!("SELECT format_type({oid}, -1)")),
            vec![want.to_string()],
            "oid {oid}"
        );
    }
}
