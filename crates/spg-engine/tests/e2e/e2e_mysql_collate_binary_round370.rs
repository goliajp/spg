//! read01 round 370 (MySQL differential, M4 P4a) — an explicit
//! `COLLATE utf8mb4_bin` on a column overrides the folding default and
//! compares / de-dups BYTE-WISE, across every read and write path.
//!
//! P2/P3 (r364/r365) made the MySQL default collation fold case and
//! accent everywhere. A column declared `COLLATE utf8mb4_bin`, though,
//! is byte-wise — but SPG kept folding it (a silent data-integrity bug:
//! `'a'` and `'A'` de-dup as one when the schema asked to keep them
//! apart). The two cases both resolve to `Collation::Binary`, so the
//! parser now records whether the clause was explicit; a MySQL text
//! column with NO clause stores `CaseInsensitive` (folds) while an
//! explicit `COLLATE utf8mb4_bin` stores `Binary` (byte-wise), and every
//! fold site — comparison, IN, LIKE, GROUP BY, DISTINCT, UNIQUE — skips
//! the fold for a `Binary` column.
//!
//! Every expectation is copied from a MariaDB 11 run.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn mysql() -> Engine {
    let mut e = Engine::new();
    e.execute("SET sql_mode='STRICT_TRANS_TABLES'").unwrap();
    e
}

fn count(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            Value::BigInt(n) => n,
            ref other => panic!("`{sql}` not a count: {other:?}"),
        },
        other => panic!("`{sql}` no rows: {other:?}"),
    }
}

/// Seed `{a, A, bar, Bar}` into a table with the given column DDL.
fn seed(ddl: &str) -> Engine {
    let mut e = mysql();
    e.execute(ddl).unwrap();
    e.execute("INSERT INTO t VALUES ('a'),('A'),('bar'),('Bar')")
        .unwrap();
    e
}

/// Every read path is byte-wise on an explicit-binary column.
#[test]
fn explicit_binary_column_is_byte_wise_everywhere() {
    let mut e = seed("CREATE TABLE t (t VARCHAR(10) COLLATE utf8mb4_bin)");
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM t WHERE t = 'a'"), 1);
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM t WHERE t IN ('A')"), 1);
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM t WHERE t LIKE 'bar'"), 1);
    assert_eq!(count(&mut e, "SELECT COUNT(DISTINCT t) FROM t"), 4);
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM (SELECT t FROM t GROUP BY t) g"),
        4
    );
}

/// A column with NO explicit collation keeps the folding default.
#[test]
fn default_column_still_folds() {
    let mut e = seed("CREATE TABLE t (t VARCHAR(10))");
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM t WHERE t = 'a'"), 2);
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM t WHERE t IN ('A')"), 2);
    assert_eq!(count(&mut e, "SELECT COUNT(*) FROM t WHERE t LIKE 'bar'"), 2);
    assert_eq!(count(&mut e, "SELECT COUNT(DISTINCT t) FROM t"), 2);
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM (SELECT t FROM t GROUP BY t) g"),
        2
    );
}

/// UNIQUE on an explicit-binary column keeps both byte-distinct values;
/// a default UNIQUE column rejects the case variant.
#[test]
fn unique_respects_the_explicit_collation() {
    let mut b = mysql();
    b.execute("CREATE TABLE t (t VARCHAR(10) COLLATE utf8mb4_bin UNIQUE)")
        .unwrap();
    b.execute("INSERT INTO t VALUES ('a')").unwrap();
    assert!(
        b.execute("INSERT INTO t VALUES ('A')").is_ok(),
        "binary UNIQUE keeps 'a' and 'A'"
    );

    let mut d = mysql();
    d.execute("CREATE TABLE t (t VARCHAR(10) UNIQUE)").unwrap();
    d.execute("INSERT INTO t VALUES ('a')").unwrap();
    assert!(
        d.execute("INSERT INTO t VALUES ('A')").is_err(),
        "default UNIQUE folds 'a' = 'A'"
    );
}

/// A PostgreSQL session is byte-wise regardless — the dialect gate never
/// engages, so an explicit collation clause changes nothing there.
#[test]
fn postgres_session_unchanged() {
    let mut p = Engine::new();
    p.execute("CREATE TABLE t (t TEXT)").unwrap();
    p.execute("INSERT INTO t VALUES ('a'),('A')").unwrap();
    assert_eq!(count(&mut p, "SELECT COUNT(*) FROM t WHERE t = 'a'"), 1);
    assert_eq!(count(&mut p, "SELECT COUNT(DISTINCT t) FROM t"), 2);
}
