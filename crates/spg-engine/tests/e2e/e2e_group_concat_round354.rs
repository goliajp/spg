//! read01 round 354 (MySQL differential, M12) — GROUP_CONCAT's clauses.
//!
//! `ORDER BY` and `DISTINCT` inside GROUP_CONCAT already worked. Two
//! things did not:
//!
//!   * `SEPARATOR '<s>'` was a parse error, so every MySQL query that
//!     names its own separator failed outright. (And once it parsed, the
//!     separator was being DROPPED — the aggregate only read a second
//!     argument for `string_agg` — so `SEPARATOR '|'` quietly kept the
//!     default comma until that was fixed too.)
//!   * MySQL's GROUP_CONCAT takes ANY number of value arguments and
//!     concatenates them per row: `GROUP_CONCAT(n, ':', t)` is
//!     `3:c,1:a,2:b,1:a` (measured on MariaDB 11), not a second argument
//!     meaning a separator — that is what the SEPARATOR tail is for.
//!
//! Every expectation is copied from the MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE gc (g INT, n INT, t VARCHAR(10))")
        .unwrap();
    e.execute(
        "INSERT INTO gc VALUES (1,3,'c'),(1,1,'a'),(1,2,'b'),(1,1,'a'),(2,9,'z'),(2,NULL,NULL)",
    )
    .unwrap();
    e
}

fn one(e: &mut Engine, sql: &str) -> Value<'static> {
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

#[test]
fn separator_is_honoured() {
    let mut e = fixture();
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(t SEPARATOR '|') FROM gc WHERE g=1"),
        Value::text("c|a|b|a"),
    );
    // The empty separator really is empty, not "fall back to a comma".
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(t SEPARATOR '') FROM gc WHERE g=1"),
        Value::text("caba"),
    );
    // …and it composes with ORDER BY, which comes first.
    assert_eq!(
        one(
            &mut e,
            "SELECT GROUP_CONCAT(t ORDER BY n DESC SEPARATOR '|') FROM gc WHERE g=1"
        ),
        Value::text("c|b|a|a"),
    );
}

/// Several value arguments concatenate PER ROW.
#[test]
fn many_arguments_concatenate_per_row() {
    let mut e = fixture();
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(n, ':', t) FROM gc WHERE g=1"),
        Value::text("3:c,1:a,2:b,1:a"),
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT GROUP_CONCAT(n, ':', t SEPARATOR '|') FROM gc WHERE g=1"
        ),
        Value::text("3:c|1:a|2:b|1:a"),
    );
}

/// What already worked has to keep working.
#[test]
fn the_defaults_are_unchanged() {
    let mut e = fixture();
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(t) FROM gc WHERE g=1"),
        Value::text("c,a,b,a"),
        "default separator is a comma, in insertion order"
    );
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(t ORDER BY n) FROM gc WHERE g=1"),
        Value::text("a,a,b,c"),
    );
    assert_eq!(
        one(
            &mut e,
            "SELECT GROUP_CONCAT(DISTINCT t ORDER BY t) FROM gc WHERE g=1"
        ),
        Value::text("a,b,c"),
    );
    // NULLs are skipped; an empty group is NULL.
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(n) FROM gc WHERE g=2"),
        Value::text("9")
    );
    assert_eq!(
        one(&mut e, "SELECT GROUP_CONCAT(t) FROM gc WHERE g=99"),
        Value::Null
    );
}

/// PG's own two-argument `string_agg` is untouched by any of this.
#[test]
fn string_agg_still_takes_its_separator() {
    let mut p = Engine::new();
    p.execute("CREATE TABLE s (t TEXT)").unwrap();
    p.execute("INSERT INTO s VALUES ('a'),('b')").unwrap();
    assert_eq!(
        one(&mut p, "SELECT string_agg(t, '-') FROM s"),
        Value::text("a-b")
    );
}
