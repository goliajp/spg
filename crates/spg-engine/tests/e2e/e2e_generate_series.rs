//! v7.17.0 Phase 3.10 — `FROM generate_series(start, stop [,
//! step])` set-returning source. Integer + INTERVAL shapes.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn integer_two_arg_default_step() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT * FROM generate_series(1, 5)").unwrap());
    let vals: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::BigInt(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(vals, vec![1, 2, 3, 4, 5]);
}

#[test]
fn integer_three_arg_step() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT * FROM generate_series(1, 10, 2)")
            .unwrap(),
    );
    let vals: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::BigInt(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(vals, vec![1, 3, 5, 7, 9]);
}

#[test]
fn integer_negative_step_descending() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT * FROM generate_series(5, 1, -1)")
            .unwrap(),
    );
    let vals: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::BigInt(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(vals, vec![5, 4, 3, 2, 1]);
}

#[test]
fn integer_empty_range() {
    let mut e = Engine::new();
    // start > stop with positive step → empty result.
    let r = rows(e.execute("SELECT * FROM generate_series(10, 5)").unwrap());
    assert!(r.is_empty());
}

#[test]
fn integer_step_zero_errors() {
    let mut e = Engine::new();
    let r = e.execute("SELECT * FROM generate_series(1, 5, 0)");
    assert!(r.is_err());
}

#[test]
fn integer_with_alias_and_where() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT * FROM generate_series(1, 10) AS s WHERE s > 7")
            .unwrap(),
    );
    let vals: Vec<i64> = r
        .iter()
        .map(|row| match row[0] {
            Value::BigInt(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(vals, vec![8, 9, 10]);
}

#[test]
fn timestamp_interval_one_day() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT * FROM generate_series(\
                '2024-01-01 00:00:00'::TIMESTAMP, \
                '2024-01-04 00:00:00'::TIMESTAMP, \
                '1 day'::INTERVAL\
             )",
        )
        .unwrap(),
    );
    // Expect 4 rows (Jan 1, 2, 3, 4 — both endpoints inclusive).
    assert_eq!(r.len(), 4);
    // First row is the start.
    assert_eq!(
        r[0][0],
        Value::Timestamp(parse_iso_to_micros("2024-01-01 00:00:00"))
    );
    // Last row is Jan 4.
    assert_eq!(
        r[3][0],
        Value::Timestamp(parse_iso_to_micros("2024-01-04 00:00:00"))
    );
}

#[test]
fn timestamp_interval_one_hour_count_only() {
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT * FROM generate_series(\
                '2024-01-01 00:00:00'::TIMESTAMP, \
                '2024-01-01 03:00:00'::TIMESTAMP, \
                '1 hour'::INTERVAL\
             )",
        )
        .unwrap(),
    );
    // 00:00 / 01:00 / 02:00 / 03:00 → 4 rows.
    assert_eq!(r.len(), 4);
}

#[test]
fn unsupported_arg_shape_errors_cleanly() {
    let mut e = Engine::new();
    // Mixed shape (timestamp + integer) — should error, not panic.
    let r = e.execute("SELECT * FROM generate_series('2024-01-01'::TIMESTAMP, 5)");
    assert!(r.is_err());
}

/// 2024-epoch parser helper for absolute-comparison assertions —
/// the engine uses microseconds-since-epoch internally.
fn parse_iso_to_micros(s: &str) -> i64 {
    // Quick-and-dirty: parse YYYY-MM-DD HH:MM:SS via chrono-free
    // math. The Engine does the real parsing; we re-derive here
    // for the test-only comparison.
    let bytes = s.as_bytes();
    let y: i32 = std::str::from_utf8(&bytes[0..4]).unwrap().parse().unwrap();
    let mo: u32 = std::str::from_utf8(&bytes[5..7]).unwrap().parse().unwrap();
    let d: u32 = std::str::from_utf8(&bytes[8..10]).unwrap().parse().unwrap();
    let h: u32 = std::str::from_utf8(&bytes[11..13])
        .unwrap()
        .parse()
        .unwrap();
    let mi: u32 = std::str::from_utf8(&bytes[14..16])
        .unwrap()
        .parse()
        .unwrap();
    let se: u32 = std::str::from_utf8(&bytes[17..19])
        .unwrap()
        .parse()
        .unwrap();
    // Days from epoch using Gregorian. For 2024 dates only we
    // hard-code the leap-year-aware day-of-year table.
    const DAYS_PER_MONTH_LEAP: [i64; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    const DAYS_PER_MONTH: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = |y: i32| -> bool { (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0) };
    let mut days_since_epoch: i64 = 0;
    for yr in 1970..y {
        days_since_epoch += if is_leap(yr) { 366 } else { 365 };
    }
    let dom = if is_leap(y) {
        &DAYS_PER_MONTH_LEAP
    } else {
        &DAYS_PER_MONTH
    };
    for m in 0..(mo as usize - 1) {
        days_since_epoch += dom[m];
    }
    days_since_epoch += (d as i64) - 1;
    let secs = days_since_epoch * 86_400 + h as i64 * 3600 + mi as i64 * 60 + se as i64;
    secs * 1_000_000
}
