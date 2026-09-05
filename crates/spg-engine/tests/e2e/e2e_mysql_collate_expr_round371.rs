//! read01 round 371 (MySQL differential, M4 P4b) — a per-expression
//! `… COLLATE utf8mb4_bin` overrides the folding default and compares /
//! sorts / de-dups BYTE-WISE, on the expression it is attached to.
//!
//! P4a (r370) honoured a column's declared collation; P4b honours an
//! explicit override written on an expression. SPG used to REJECT the
//! clause outright (`COLLATE utf8mb4_bin: locale collations are not
//! supported`), failing every query that carried one. It now lowers a
//! `_bin` override onto the same representation as `BINARY expr`, so
//! every fold site (comparison, IN, LIKE, GROUP BY, DISTINCT, ORDER BY)
//! suppresses the fold; a `_ci` override folds (the dialect default), so
//! it absorbs as a no-op.
//!
//! v7.40.0 — the lowering onto `BINARY` is gone, and the reason is in
//! `e2e_mysql_face_v7400`: byte-wise is two properties, and the BINARY
//! cast carries both. `utf8mb4_bin` does not fold AND it PADS.
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

fn count(e: &mut Engine, sql: &str) -> i64 {
    match scalar(e, sql) {
        Value::BigInt(n) => n,
        other => panic!("`{sql}` not a count: {other:?}"),
    }
}

/// `COLLATE utf8mb4_bin` on either operand forces a byte-wise compare;
/// `COLLATE utf8mb4_general_ci` folds.
#[test]
fn expression_collate_overrides_the_comparison() {
    let mut e = mysql();
    assert_eq!(
        scalar(&mut e, "SELECT 'a' = 'A' COLLATE utf8mb4_bin"),
        Value::Bool(false)
    );
    assert_eq!(
        scalar(&mut e, "SELECT 'a' COLLATE utf8mb4_bin = 'A'"),
        Value::Bool(false)
    );
    assert_eq!(
        scalar(&mut e, "SELECT 'a' COLLATE utf8mb4_general_ci = 'A'"),
        Value::Bool(true)
    );
}

/// WHERE / IN / LIKE / DISTINCT all honour a `COLLATE utf8mb4_bin`.
#[test]
fn every_clause_honours_the_override() {
    let mut e = mysql();
    e.execute("CREATE TABLE ci (t VARCHAR(10))").unwrap();
    e.execute("INSERT INTO ci VALUES ('a'),('A'),('b'),('B')")
        .unwrap();
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM ci WHERE t = 'a' COLLATE utf8mb4_bin"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM ci WHERE t IN ('A' COLLATE utf8mb4_bin)"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM ci WHERE t LIKE 'a' COLLATE utf8mb4_bin"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(DISTINCT t COLLATE utf8mb4_bin) FROM ci"
        ),
        4
    );
    // Without the override the dialect default still folds.
    assert_eq!(count(&mut e, "SELECT COUNT(DISTINCT t) FROM ci"), 2);
}

/// ORDER BY under the override sorts byte-wise (uppercase before
/// lowercase), measured on MariaDB via GROUP_CONCAT.
#[test]
fn order_by_override_is_byte_wise() {
    let mut e = mysql();
    e.execute("CREATE TABLE ci (t VARCHAR(10))").unwrap();
    e.execute("INSERT INTO ci VALUES ('a'),('A'),('b'),('B')")
        .unwrap();
    let g = scalar(
        &mut e,
        "SELECT GROUP_CONCAT(t ORDER BY t COLLATE utf8mb4_bin SEPARATOR ',') FROM ci",
    );
    assert_eq!(g, Value::text("A,B,a,b"));
}

/// A PostgreSQL session still rejects an unknown locale collation — the
/// MySQL `_bin` / `_ci` override only resolves under the dialect (where it
/// collapses onto `BINARY` / a fold no-op). PG keeps its honest error.
#[test]
fn postgres_session_rejects_the_mysql_collation() {
    let mut p = Engine::new();
    assert!(
        p.execute("SELECT 'a' = 'A' COLLATE utf8mb4_bin").is_err(),
        "PG has no utf8mb4_bin collation"
    );
    // The byte-order spellings PG does know still absorb as a no-op.
    assert_eq!(
        scalar(&mut p, "SELECT 'a' = 'A' COLLATE \"C\""),
        Value::Bool(false)
    );
}
