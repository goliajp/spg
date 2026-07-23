//! read01 round 379 (MySQL differential) — the SHORT generated-column
//! form, `<col> <type> AS (<expr>) [STORED | VIRTUAL]`, which omits
//! `GENERATED ALWAYS`.
//!
//! MariaDB accepts a generated column written as `b INT AS (a * 2)
//! STORED` (or VIRTUAL, or neither — VIRTUAL is the default). mysqldump
//! emits the long `GENERATED ALWAYS AS (...) STORED` form, which SPG
//! already parsed, but hand-written schemas and app migrations use the
//! short one — and SPG rejected it with a syntax error at `AS`. Both
//! forms now parse and compute the same value; SPG stores the result
//! (STORED / VIRTUAL are observably identical for query results).
//!
//! Behaviour matches a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn row(e: &mut Engine, sql: &str) -> Vec<Value<'static>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => {
            rows[0].values.iter().cloned().map(Value::into_owned).collect()
        }
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// The short form parses with STORED, VIRTUAL, or no keyword.
#[test]
fn short_form_parses_and_computes() {
    let mut e = mysql();
    e.execute(
        "CREATE TABLE g (a INT, b INT AS (a * 2) STORED, \
         c INT AS (a + 1) VIRTUAL, d INT AS (a + 5))",
    )
    .unwrap();
    e.execute("INSERT INTO g (a) VALUES (5)").unwrap();
    assert_eq!(
        row(&mut e, "SELECT a, b, c, d FROM g"),
        vec![Value::Int(5), Value::Int(10), Value::Int(6), Value::Int(10)]
    );
}

/// A generated column recomputes when its base column changes.
#[test]
fn short_form_recomputes_on_update() {
    let mut e = mysql();
    e.execute("CREATE TABLE g (a INT, b INT AS (a * 2) STORED)")
        .unwrap();
    e.execute("INSERT INTO g (a) VALUES (3)").unwrap();
    e.execute("UPDATE g SET a = 7").unwrap();
    assert_eq!(
        row(&mut e, "SELECT a, b FROM g"),
        vec![Value::Int(7), Value::Int(14)]
    );
}

/// The long `GENERATED ALWAYS AS` form (what mysqldump emits) still works.
#[test]
fn long_form_still_parses() {
    let mut e = mysql();
    e.execute("CREATE TABLE g (a INT, b INT GENERATED ALWAYS AS (a * 3) STORED)")
        .unwrap();
    e.execute("INSERT INTO g (a) VALUES (4)").unwrap();
    assert_eq!(
        row(&mut e, "SELECT a, b FROM g"),
        vec![Value::Int(4), Value::Int(12)]
    );
}
