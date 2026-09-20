//! 9.0.0 — catalog columns PostgreSQL declares `oid` / `regproc` / `xid`
//! are declared so here, and the catalog agrees with itself about them.
//!
//! Driven by measurement rather than by a list of column names: every
//! `pg_*` catalog column SPG declared `bigint` was asked of PostgreSQL
//! 18.6 — 142 of them, of which 122 are `oid`, 19 `regproc` and 1 `xid`.
//! The 122 are re-declared here. A driver decodes by the announced type,
//! and `oid` is four bytes in binary where `bigint` is eight.
//!
//! The other 20 are NOT: a `regproc` column renders the function's NAME
//! on PostgreSQL, and these cells hold a number, so declaring the type
//! without changing the value would announce one thing and send another.
//!
//! The same pass caught the catalog's own type map missing `oid` while
//! the wire's map had it: a column declared `oid` reported `atttypid` 0,
//! and `format_type` over it answered `???`. The second test asks the
//! engine to agree with itself over every catalog column, which is the
//! shape that defect has.

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

#[test]
fn catalog_identifier_columns_carry_pg_s_declared_type() {
    let mut e = Engine::new();
    for (sql, want) in [
        ("SELECT pg_typeof(oid) FROM pg_class LIMIT 1", "oid"),
        (
            "SELECT pg_typeof(attrelid) FROM pg_attribute LIMIT 1",
            "oid",
        ),
        (
            "SELECT pg_typeof(atttypid) FROM pg_attribute LIMIT 1",
            "oid",
        ),
        ("SELECT pg_typeof(typelem) FROM pg_type LIMIT 1", "oid"),
        ("SELECT pg_typeof(prorettype) FROM pg_proc LIMIT 1", "oid"),
        // What was already right stays right.
        (
            "SELECT pg_typeof(relnatts) FROM pg_class LIMIT 1",
            "smallint",
        ),
    ] {
        assert_eq!(col(&mut e, sql), vec![want.to_string()], "{sql}");
    }
}

#[test]
fn every_catalog_column_type_has_a_name() {
    let mut e = Engine::new();
    let unnamed = col(
        &mut e,
        "SELECT c.relname || '.' || a.attname FROM pg_class c \
         JOIN pg_attribute a ON a.attrelid = c.oid \
         WHERE c.relname LIKE 'pg\\_%' AND a.attnum > 0 \
           AND format_type(a.atttypid, -1) = '???' ORDER BY 1",
    );
    // Vacuous over an empty catalog — make sure there is one to walk.
    let total = col(
        &mut e,
        "SELECT count(*)::text FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid \
         WHERE c.relname LIKE 'pg\\_%' AND a.attnum > 0",
    );
    assert!(
        total[0].parse::<usize>().expect("a count") >= 300,
        "only {} catalog columns",
        total[0]
    );
    assert!(unnamed.is_empty(), "atttypid names no type: {unnamed:?}");
}
