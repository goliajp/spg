//! read01 round 374 (MySQL differential) — `CONCAT(...)` returns NULL
//! when ANY argument is NULL under the MySQL dialect.
//!
//! MariaDB 11: `CONCAT('a', NULL, 'b')` is NULL — a NULL poisons the
//! whole result. PG's `CONCAT` skips NULLs (`'ab'`). SPG followed PG in
//! both dialects, so a MySQL query like `CONCAT(first, ' ', last)` with a
//! NULL column silently produced the concatenation of the non-NULL parts
//! instead of NULL. `CONCAT_WS` keeps skipping NULLs in both dialects
//! (that is its whole point), so it is unchanged.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn scalar(e: &mut Engine, sql: &str) -> Value<'static> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.values.first())
            .cloned()
            .map(Value::into_owned)
            .unwrap_or(Value::Null),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

/// A NULL argument poisons CONCAT to NULL.
#[test]
fn concat_with_null_is_null() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT CONCAT('a', NULL, 'b')"), Value::Null);
    assert_eq!(scalar(&mut e, "SELECT CONCAT(NULL)"), Value::Null);
    // No NULL: ordinary concatenation.
    assert_eq!(scalar(&mut e, "SELECT CONCAT('a', 'b')"), Value::text("ab"));
    assert_eq!(scalar(&mut e, "SELECT CONCAT(1, 2, 3)"), Value::text("123"));
}

/// CONCAT_WS still skips NULLs — the separator is not repeated around
/// them, and the result is not NULL.
#[test]
fn concat_ws_still_skips_nulls() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT CONCAT_WS(',', 'a', NULL, 'b')"),
        Value::text("a,b")
    );
}

/// A PostgreSQL session keeps PG's skip-NULL CONCAT.
#[test]
fn postgres_session_skips_nulls() {
    let mut p = Engine::new();
    assert_eq!(
        scalar(&mut p, "SELECT CONCAT('a', NULL, 'b')"),
        Value::text("ab")
    );
}
