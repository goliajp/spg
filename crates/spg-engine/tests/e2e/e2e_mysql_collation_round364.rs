//! read01 round 364 (MySQL differential, M4 P2) — the default
//! collation is case- AND accent-insensitive on the READ path.
//!
//! MariaDB 11's server default is `utf8mb4_uca1400_ai_ci`: comparisons,
//! `WHERE`, joins, `IN`, `LIKE`, `GROUP BY`, `DISTINCT` and `ORDER BY`
//! all fold case and strip accents, so `'foo' = 'Foo' = 'FOO'` and
//! `'bar' = 'Bär'`. SPG used to compare these byte-wise in the MySQL
//! dialect, which is silently wrong for every one of those clauses at
//! once — the point of P2 is that they now ALL fold together (a query
//! whose `WHERE` folds but whose `GROUP BY` does not is self-
//! contradictory). `BINARY x` still forces a byte-wise comparison, and
//! a PG session is completely unaffected.
//!
//! Every expectation is copied from a MariaDB 11 run of the same
//! statements (default collation `utf8mb4_uca1400_ai_ci`).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

/// A MySQL-dialect engine (`SET sql_mode` flips `backslash_escapes`,
/// which is the dialect signal the whole read path keys off).
fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e.execute("CREATE TABLE ci4 (t VARCHAR(10))").unwrap();
    // 3 foo-variants, 2 bar-variants (one accented).
    e.execute("INSERT INTO ci4 VALUES ('foo'),('Foo'),('FOO'),('bar'),('Bär')")
        .unwrap();
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

fn text(e: &mut Engine, sql: &str) -> String {
    match scalar(e, sql) {
        Value::Text(s) => s.into_owned(),
        other => panic!("`{sql}` was not text: {other:?}"),
    }
}

/// Bare `=` folds case and accent; `BINARY` forces byte-wise.
#[test]
fn comparison_folds_case_and_accent() {
    let mut e = mysql();
    assert_eq!(scalar(&mut e, "SELECT 'a' = 'A'"), Value::Bool(true));
    assert_eq!(scalar(&mut e, "SELECT 'Bar' = 'Bär'"), Value::Bool(true));
    // BINARY on either side turns folding back off.
    assert_eq!(scalar(&mut e, "SELECT BINARY 'a' = 'A'"), Value::Bool(false));
    assert_eq!(
        scalar(&mut e, "SELECT 'Bar' = BINARY 'Bär'"),
        Value::Bool(false)
    );
}

/// The clauses that read text all fold together — this is the whole
/// point of P2. WHERE, IN, LIKE, DISTINCT, GROUP BY.
#[test]
fn every_read_clause_folds_consistently() {
    let mut e = mysql();
    // WHERE matches all 3 foo-variants.
    assert_eq!(
        scalar(&mut e, "SELECT COUNT(*) FROM ci4 WHERE t = 'foo'"),
        Value::BigInt(3)
    );
    // IN matches the same 3.
    assert_eq!(
        scalar(&mut e, "SELECT COUNT(*) FROM ci4 WHERE t IN ('FOO')"),
        Value::BigInt(3)
    );
    // LIKE with no wildcards is a folded equality.
    assert_eq!(
        scalar(&mut e, "SELECT 'Foo' LIKE 'foo'"),
        Value::Bool(true)
    );
    // DISTINCT collapses to 2 folded groups (foo*, bar*).
    assert_eq!(
        scalar(&mut e, "SELECT COUNT(DISTINCT t) FROM ci4"),
        Value::BigInt(2)
    );
    // GROUP BY makes the same 2 groups.
    assert_eq!(
        scalar(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT t FROM ci4 GROUP BY t) g"
        ),
        Value::BigInt(2)
    );
}

/// GROUP_CONCAT's internal ORDER BY folds too: the two accent/case
/// variants of "bar" sort as one contiguous block ahead of the three
/// "foo" variants (MariaDB `Bär,bar,FOO,Foo,foo`). The tie order WITHIN
/// a fold-equal block is implementation-defined in MariaDB, so the pin
/// asserts the fold PROPERTY — bar-block first and contiguous, foo-block
/// second and contiguous — not an exact tie order.
#[test]
fn aggregate_order_by_folds() {
    let mut e = mysql();
    let g = text(&mut e, "SELECT GROUP_CONCAT(t ORDER BY t SEPARATOR ',') FROM ci4");
    let parts: Vec<&str> = g.split(',').collect();
    assert_eq!(parts.len(), 5, "all five rows present: {g}");
    let is_bar = |s: &str| s.eq_ignore_ascii_case("bar") || s == "Bär";
    assert!(is_bar(parts[0]) && is_bar(parts[1]), "bar-block leads: {g}");
    assert!(
        parts[2..].iter().all(|s| s.eq_ignore_ascii_case("foo")),
        "foo-block trails and is contiguous: {g}"
    );
}

/// `ORDER BY BINARY t` inside GROUP_CONCAT is fully deterministic —
/// byte order, uppercase before lowercase (measured on MariaDB 11).
#[test]
fn aggregate_binary_order_by_is_byte_wise() {
    let mut e = mysql();
    assert_eq!(
        text(
            &mut e,
            "SELECT GROUP_CONCAT(t ORDER BY BINARY t SEPARATOR ',') FROM ci4"
        ),
        "Bär,FOO,Foo,bar,foo"
    );
}

/// A PostgreSQL session (no `sql_mode`) keeps byte-wise semantics — the
/// dialect gate must not leak folding into PG.
#[test]
fn postgres_session_is_unaffected() {
    let mut p = Engine::new();
    p.execute("CREATE TABLE ci4 (t TEXT)").unwrap();
    p.execute("INSERT INTO ci4 VALUES ('foo'),('Foo'),('FOO'),('bar')")
        .unwrap();
    assert_eq!(scalar(&mut p, "SELECT 'a' = 'A'"), Value::Bool(false));
    assert_eq!(
        scalar(&mut p, "SELECT COUNT(*) FROM ci4 WHERE t = 'foo'"),
        Value::BigInt(1)
    );
    assert_eq!(
        scalar(&mut p, "SELECT COUNT(DISTINCT t) FROM ci4"),
        Value::BigInt(4)
    );
}
