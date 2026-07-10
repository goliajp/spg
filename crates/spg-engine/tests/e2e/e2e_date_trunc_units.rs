//! date_trunc unit completion: week, quarter, decade, century,
//! millennium, milliseconds, microseconds.

use spg_engine::{Engine, QueryResult};

fn ts(e: &mut Engine, sql: &str) -> i64 {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match rows[0].values[0] {
        spg_storage::Value::Timestamp(t) => t,
        ref other => panic!("{sql}: expected Timestamp, got {other:?}"),
    }
}

// Micros since epoch for a plain date at midnight.
fn day(y: i32, m: u32, d: u32) -> i64 {
    // Reuse the engine to compute the reference so the test is
    // independent of the civil-day arithmetic under test.
    let mut e = Engine::new();
    ts(
        &mut e,
        &format!("SELECT date_trunc('day', DATE '{y:04}-{m:02}-{d:02}')"),
    )
}

#[test]
fn week_truncates_to_monday() {
    let mut e = Engine::new();
    // 2024-05-15 is a Wednesday; the ISO week starts Monday 2024-05-13.
    assert_eq!(
        ts(&mut e, "SELECT date_trunc('week', DATE '2024-05-15')"),
        day(2024, 5, 13)
    );
    // A Monday truncates to itself.
    assert_eq!(
        ts(&mut e, "SELECT date_trunc('week', DATE '2024-05-13')"),
        day(2024, 5, 13)
    );
}

#[test]
fn quarter_and_larger() {
    let mut e = Engine::new();
    assert_eq!(
        ts(&mut e, "SELECT date_trunc('quarter', DATE '2024-05-15')"),
        day(2024, 4, 1)
    );
    assert_eq!(
        ts(&mut e, "SELECT date_trunc('decade', DATE '2024-05-15')"),
        day(2020, 1, 1)
    );
    // 2024 is in century 21 (2001-2100).
    assert_eq!(
        ts(&mut e, "SELECT date_trunc('century', DATE '2024-05-15')"),
        day(2001, 1, 1)
    );
    // Millennium 3 is 2001-3000.
    assert_eq!(
        ts(&mut e, "SELECT date_trunc('millennium', DATE '2024-05-15')"),
        day(2001, 1, 1)
    );
}

#[test]
fn subsecond_units() {
    let mut e = Engine::new();
    let base = ts(
        &mut e,
        "SELECT date_trunc('milliseconds', TIMESTAMP '2024-01-01 00:00:00.123456')",
    );
    let day0 = day(2024, 1, 1);
    // Milliseconds keeps 123ms, drops the 456µs.
    assert_eq!(base - day0, 123_000);
    let micro = ts(
        &mut e,
        "SELECT date_trunc('microseconds', TIMESTAMP '2024-01-01 00:00:00.123456')",
    );
    assert_eq!(micro - day0, 123_456);
    // Unknown unit still errors.
    assert!(
        e.execute("SELECT date_trunc('fortnight', DATE '2024-01-01')")
            .is_err()
    );
}
