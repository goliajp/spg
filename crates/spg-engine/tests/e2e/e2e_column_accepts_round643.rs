//! v7.39 (round 643, F32) — one rule for "does this value belong in
//! this column", where there were three.
//!
//! `insert`, `update_row` and the standalone row validator each carried
//! their own copy of the storage-shape compatibility matrix, and the
//! three had drifted apart in three independent places:
//!
//!   * only `insert` accepted the `name` pairs (`Text` into a NAME
//!     column and back),
//!   * only `insert` and `update_row` accepted a bit-to-bit pair whose
//!     typmods differ,
//!   * only `update_row` accepted a NEGATIVE declared numeric scale.
//!
//! Every one of those is an omission rather than a deliberate
//! tightening, so the three now ask one function and it holds the union.
//!
//! **This fixes nothing observable, and that was checked before doing
//! it.** Each shape the three disagreed about answers identically to
//! PG18 today — the pins below are the measurement, kept so the next
//! type does not have to rediscover which of three places to patch.
//! Adding `xid` in round 640 meant remembering all three; forgetting one
//! would have half-wired it.
//!
//! The other two F32 clusters were measured in the same round and are
//! NOT converged, for reasons recorded rather than assumed:
//!
//!   * The four value comparators have genuinely different fallbacks,
//!     and the fallback is what covers each one's missing arms. Probed
//!     across UUID / bytea / macaddr / time / money / inet under both
//!     ORDER BY and min/max, including a TIME pair built to break a
//!     Debug-string ordering — SPG matched PG on all of them. A union
//!     would change behaviour where a fallback currently answers. The
//!     note now lives on `orderby::value_cmp`.
//!   * The four `sum` accumulators cover the same variant set already;
//!     the last drift there was closed when `Value::SmallInt` was added
//!     to the fourth.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

/// A NAME column stores a `Value::Text`, so both the insert path and
/// the update path have to accept one. Only insert's copy said so.
#[test]
fn round643_a_name_column_takes_text_on_every_path() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nb (a name, b INT)").unwrap();
    e.execute("INSERT INTO nb VALUES ('x', 1)").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM nb"), "x");
    e.execute("UPDATE nb SET a = 'yy' WHERE b = 1").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM nb"), "yy");
    // Through an expression, so the value is built rather than literal.
    e.execute("UPDATE nb SET a = a || 'z' WHERE b = 1").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM nb"), "yyz");
    assert_eq!(one(&mut e, "SELECT pg_typeof(a) FROM nb"), "name");
    e.execute("DELETE FROM nb WHERE a = 'yyz'").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM nb"), "0");
}

/// A negative declared scale rounds the value to a multiple of 10^|s|
/// and stores it at display scale 0, so the value's scale and the
/// column's legitimately differ. Only update's copy allowed it.
#[test]
fn round643_a_negative_numeric_scale_survives_both_paths() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ng (a numeric(10,-2))").unwrap();
    e.execute("INSERT INTO ng VALUES (12345)").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM ng"), "12300");
    e.execute("UPDATE ng SET a = 67890").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM ng"), "67900");
}

/// Two bit values whose typmods differ share the BitString shape. The
/// length contract belongs to coercion, which has already run.
#[test]
fn round643_a_bit_column_takes_a_bit_value_on_every_path() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bt (a bit(3))").unwrap();
    e.execute("INSERT INTO bt VALUES (B'101')").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM bt"), "101");
    e.execute("UPDATE bt SET a = B'110'").unwrap();
    assert_eq!(one(&mut e, "SELECT a FROM bt"), "110");
}

/// The pairs round 640 added, on every path rather than the one that
/// happened to be exercised first.
#[test]
fn round643_an_xid_column_takes_its_value_on_every_path() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE xt (a xid, b xid8, c INT)").unwrap();
    e.execute("INSERT INTO xt VALUES ('11', '12', 1)").unwrap();
    assert_eq!(one(&mut e, "SELECT a, b FROM xt"), "11|12");
    e.execute("UPDATE xt SET a = '13', b = '14' WHERE c = 1")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT a, b FROM xt"), "13|14");
}

/// And what the rule still refuses, so converging did not turn the
/// check into a rubber stamp.
#[test]
fn round643_the_rule_still_refuses() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE r (a INT)").unwrap();
    assert!(
        e.execute("INSERT INTO r VALUES ('not a number')").is_err(),
        "text into an INT column must still be refused"
    );
    e.execute("CREATE TABLE r2 (a xid)").unwrap();
    assert!(
        e.execute("INSERT INTO r2 VALUES (13)").is_err(),
        "a bare integer into an xid column must still be refused"
    );
}
