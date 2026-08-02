//! TABLE name shorthand + COLLATE clause.
//!
//! v7.39 (round 691) — this file used to pin "a locale collation errors
//! honestly", which was true while SPG had one text ordering. It performs
//! them now, on an ORDER BY key, so the pin below moved from the error to
//! the ORDER it produces. A locale collation at a COMPARISON still errors,
//! and that is pinned in `e2e_collate_explicit_round691.rs` along with why.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.clone()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn table_shorthand_selects_star() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE th (v INT, t TEXT)").unwrap();
    e.execute("INSERT INTO th VALUES (2, 'b'), (1, 'a')")
        .unwrap();
    let got = rows(&mut e, "TABLE th ORDER BY v LIMIT 1");
    assert_eq!(got.len(), 1);
    assert_eq!(as_i64(&got[0][0]), 1);
    // Set-op chain composes like any SELECT head.
    let got = rows(&mut e, "TABLE th UNION ALL TABLE th");
    assert_eq!(got.len(), 4);
}

#[test]
fn collate_c_absorbs_and_a_locale_orders() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE co (t TEXT)").unwrap();
    e.execute("INSERT INTO co VALUES ('b'), ('a')").unwrap();
    let got = rows(&mut e, "SELECT t FROM co ORDER BY t COLLATE \"C\"");
    assert_eq!(got.len(), 2);
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "a"));
    // POSIX / default spellings absorb too.
    rows(&mut e, "SELECT t COLLATE \"POSIX\" FROM co");
    rows(&mut e, "SELECT t COLLATE \"default\" FROM co");
    // v7.39 (round 691) — a locale collation on an ORDER BY key is
    // performed. On this data it agrees with byte order; the pin that shows
    // it CHANGES the answer lives in `e2e_collate_explicit_round691.rs`.
    let got = rows(&mut e, "SELECT t FROM co ORDER BY t COLLATE \"en_US\"");
    assert_eq!(got.len(), 2);
    assert!(matches!(&got[0][0], spg_storage::Value::Text(s) if s == "a"));
}
