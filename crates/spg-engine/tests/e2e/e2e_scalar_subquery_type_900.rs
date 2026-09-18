//! 9.0.0 — a scalar subquery that answers NULL keeps its type.
//!
//! Reported by sentori (§3.9). An uncorrelated scalar subquery is
//! resolved once and its value written back into the expression as a
//! literal; a NULL carries no type, so the column's was lost:
//!
//! ```text
//!   CREATE TABLE t(id int);
//!   SELECT pg_typeof((SELECT id FROM t WHERE id = 1));
//!     PG 18.6   integer      SPG 8.0.4   unknown
//! ```
//!
//! PostgreSQL's scalar subquery node keeps the column's type whether or
//! not a row came back. Writing the declaration into the expression is
//! what the neighbouring arms already do for the types a VALUE cannot
//! carry (timestamptz, jsonb). Every expectation is PG 18.6's.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .map(spg_engine::eval::value_to_text)
            .unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn an_empty_scalar_subquery_reports_its_columns_type() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE sst(id int, s text, n numeric(8,2), t timestamptz, \
         b bytea, u uuid, j jsonb, a int[])",
    )
    .unwrap();
    for (col, want) in [
        ("id", "integer"),
        ("s", "text"),
        ("n", "numeric"),
        ("t", "timestamp with time zone"),
        ("b", "bytea"),
        ("u", "uuid"),
        ("j", "jsonb"),
        ("a", "integer[]"),
    ] {
        let sql = format!("SELECT pg_typeof((SELECT {col} FROM sst WHERE id = 1))");
        assert_eq!(one(&mut e, &sql), want, "{sql}");
    }
}

/// A row that DOES come back still reports what it did, and the NULL
/// still behaves as a NULL.
#[test]
fn a_row_that_comes_back_and_a_null_still_behave() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE sst(id int)").unwrap();
    assert_eq!(
        one(&mut e, "SELECT (SELECT id FROM sst WHERE id = 1) IS NULL"),
        "true"
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT coalesce((SELECT id FROM sst WHERE id = 1), 7)"
        ),
        "7"
    );
    e.execute("INSERT INTO sst VALUES (1)").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT pg_typeof((SELECT id FROM sst WHERE id = 1))"
        ),
        "integer"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof((SELECT count(*) FROM sst))"),
        "bigint"
    );
}
