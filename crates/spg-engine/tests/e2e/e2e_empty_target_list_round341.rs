//! read01 round 341 (V66) — PG's target list may be empty.
//!
//! `SELECT FROM t` is legal PG (`opt_target_list: target_list | EMPTY`)
//! and answers one **zero-column** row per row of t; a bare `SELECT`
//! answers a single zero-column row. SPG raised a syntax error at FROM,
//! so a generated query that projects nothing — the shape an ORM emits
//! for `EXISTS`-style probes and for `count(*)` rewrites — did not run
//! at all.
//!
//! Every count below is from the PG 18.4 run: 3 / 2 / 3 / 3 / 2 / 1 / 1 / 1.
//!
//! Reaching zero columns surfaced a silent-wrong that had nothing to do
//! with the empty list: a scalar subquery's arity was never checked, so
//! `SELECT (SELECT a, b FROM t LIMIT 1)` quietly answered the FIRST
//! column where PG says `subquery must return only one column`.

use spg_engine::{Engine, QueryResult};

fn shape(e: &mut Engine, sql: &str) -> (usize, usize) {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, columns } => (rows.len(), columns.len()),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (a INT, b TEXT)").unwrap();
    e.execute("INSERT INTO t VALUES (1,'x'),(2,'y'),(3,'z')")
        .unwrap();
    e
}

/// The row count is the table's; the column count is zero.
#[test]
fn an_empty_target_list_keeps_the_rows() {
    let mut e = fixture();
    assert_eq!(shape(&mut e, "SELECT FROM t"), (3, 0));
    assert_eq!(shape(&mut e, "SELECT FROM t WHERE a > 1"), (2, 0));
    assert_eq!(shape(&mut e, "SELECT FROM t ORDER BY a LIMIT 2"), (2, 0));
    assert_eq!(shape(&mut e, "SELECT FROM t, t t2"), (9, 0));
}

/// With no FROM at all it is still one row — PG's implicit single row.
#[test]
fn a_bare_select_is_one_zero_column_row() {
    let mut e = fixture();
    assert_eq!(shape(&mut e, "SELECT"), (1, 0));
    assert_eq!(shape(&mut e, "SELECT WHERE true"), (1, 0));
}

/// Grouping and aggregation still apply to a projection of nothing.
#[test]
fn grouping_still_groups() {
    let mut e = fixture();
    assert_eq!(shape(&mut e, "SELECT FROM t GROUP BY a"), (3, 0));
    assert_eq!(shape(&mut e, "SELECT FROM t HAVING count(*) > 0"), (1, 0));
    // UNION dedupes the zero-column rows down to one.
    assert_eq!(shape(&mut e, "SELECT FROM t UNION SELECT FROM t"), (1, 0));
}

/// As a derived table it still carries its rows — count(*) sees three.
#[test]
fn it_composes_as_a_subquery() {
    let mut e = fixture();
    let r = e
        .execute("SELECT count(*) FROM (SELECT FROM t) s")
        .unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], spg_storage::Value::BigInt(3));
}

/// Where PG rejects a zero-column subquery, so must SPG — in PG's words.
#[test]
fn subquery_arity_reads_like_pg() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "SELECT 1 FROM t WHERE a IN (SELECT FROM t)"),
        "unsupported: subquery has too few columns",
    );
    assert_eq!(
        err(&mut e, "SELECT 1 FROM t WHERE a IN (SELECT a, b FROM t)"),
        "unsupported: subquery has too many columns",
    );
}

/// The silent-wrong this round surfaced: a scalar subquery projecting two
/// columns answered the first one instead of erroring.
#[test]
fn a_scalar_subquery_must_project_exactly_one_column() {
    let mut e = fixture();
    for sql in [
        "SELECT (SELECT a, b FROM t LIMIT 1)",
        "SELECT (SELECT a, b FROM t WHERE a = 1)",
        "SELECT (SELECT FROM t)",
        "SELECT 1 FROM t x WHERE a = (SELECT a, b FROM t WHERE a = x.a)",
    ] {
        assert_eq!(
            err(&mut e, sql),
            "unsupported: subquery must return only one column",
            "for `{sql}`"
        );
    }
    // The one-column form is untouched.
    let r = e.execute("SELECT (SELECT a FROM t WHERE a = 2)").unwrap();
    let QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows[0].values[0], spg_storage::Value::Int(2));
}
