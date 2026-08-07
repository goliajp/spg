//! read01 round 410 (MySQL differential) — set operations & DISTINCT
//! deduplicate by the session collation.
//!
//! MariaDB's default collation `utf8mb4_uca1400_ai_ci` is case-insensitive,
//! accent-insensitive, and PAD SPACE, so `'a'`, `'A'`, `'á'`, and `'a '`
//! collapse to one value. GROUP BY already folded its keys this way, but
//! UNION / INTERSECT / EXCEPT / SELECT DISTINCT deduplicated byte-exactly —
//! returning too many rows on a MySQL session (a silent-wrong result set).
//! Deduplication now folds text under the MySQL dialect; PostgreSQL keeps
//! its byte-exact set semantics.
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
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Int(n) => i64::from(*n),
            Value::BigInt(n) => *n,
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn one_text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            Value::Text(s) => s.to_string(),
            o => panic!("{o:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// UNION folds trailing spaces, case, and accents to one row.
#[test]
fn union_folds() {
    let mut e = mysql();
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'a' x UNION SELECT 'a ') u"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'a' x UNION SELECT 'A') u"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'e' x UNION SELECT 'é') u"
        ),
        1
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'a' x UNION SELECT 'a  ' UNION SELECT 'A') u"
        ),
        1
    );
    // Genuinely different strings are still kept.
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'ab' x UNION SELECT 'ac') u"
        ),
        2
    );
    // Numbers still dedup as before.
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM (SELECT 1 x UNION SELECT 1) u"),
        1
    );
}

/// UNION keeps the first occurrence's original value.
#[test]
fn union_keeps_first_original() {
    let mut e = mysql();
    assert_eq!(
        one_text(&mut e, "SELECT x FROM (SELECT 'A' x UNION SELECT 'a') u"),
        "A"
    );
}

/// SELECT DISTINCT, INTERSECT, EXCEPT fold too.
#[test]
fn distinct_intersect_except_fold() {
    let mut e = mysql();
    e.execute("CREATE TABLE ps(v VARCHAR(10))").unwrap();
    e.execute("INSERT INTO ps VALUES('a'),('a '),('A'),('b')")
        .unwrap();
    assert_eq!(
        count(&mut e, "SELECT COUNT(*) FROM (SELECT DISTINCT v FROM ps) t"),
        2
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT v FROM ps INTERSECT SELECT 'a') t"
        ),
        1
    );
    // {a,a ,A,b} EXCEPT {a} folds a/a /A away -> only b.
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT v FROM ps EXCEPT SELECT 'a') t"
        ),
        1
    );
}

/// A PostgreSQL session keeps byte-exact set semantics (case-sensitive,
/// space-significant).
#[test]
fn postgres_byte_exact() {
    let mut e = Engine::new();
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'a' x UNION SELECT 'a ') u"
        ),
        2
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT 'a' x UNION SELECT 'A') u"
        ),
        2
    );
    assert_eq!(
        count(
            &mut e,
            "SELECT COUNT(*) FROM (SELECT DISTINCT v FROM (VALUES('a'),('a '),('A')) t(v)) d"
        ),
        3
    );
}
