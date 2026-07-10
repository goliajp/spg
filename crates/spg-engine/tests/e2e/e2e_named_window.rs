//! WINDOW w AS (...) named windows — OVER w inlines the
//! definition at parse time.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        // Window sums surface as Float on this engine path.
        spg_storage::Value::Float(f) => *f as i64,
        other => panic!("expected number, got {other:?}"),
    }
}

#[test]
fn named_window_running_sum() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nw (v INT)").unwrap();
    e.execute("INSERT INTO nw VALUES (1), (2), (3)").unwrap();
    let got = rows(
        &mut e,
        "SELECT v, sum(v) OVER w FROM nw WINDOW w AS (ORDER BY v) ORDER BY v",
    );
    assert_eq!(got.len(), 3);
    // Running sums 1, 3, 6.
    assert_eq!(as_i64(&got[0][1]), 1);
    assert_eq!(as_i64(&got[2][1]), 6);
}

#[test]
fn two_windows_and_partition() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nw2 (g TEXT, v INT)").unwrap();
    e.execute("INSERT INTO nw2 VALUES ('a', 1), ('a', 2), ('b', 5)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT g, sum(v) OVER p, row_number() OVER o FROM nw2 \
         WINDOW p AS (PARTITION BY g), o AS (ORDER BY v) ORDER BY v",
    );
    assert_eq!(got.len(), 3);
    // Per-partition totals: a → 3, b → 5; global row numbers 1..3.
    assert_eq!(as_i64(&got[0][1]), 3);
    assert_eq!(as_i64(&got[2][1]), 5);
    assert_eq!(as_i64(&got[2][2]), 3);
}

#[test]
fn unknown_window_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nw3 (v INT)").unwrap();
    let err = e
        .execute("SELECT sum(v) OVER nope FROM nw3 WINDOW w AS (ORDER BY v)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("nope"), "unexpected error: {msg}");
}
