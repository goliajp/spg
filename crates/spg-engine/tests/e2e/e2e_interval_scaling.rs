//! INTERVAL * / numeric scaling + unary minus — PG interval_mul
//! semantics with month/day fractional-remainder spill.

use spg_engine::{Engine, QueryResult};

fn interval(e: &mut Engine, sql: &str) -> (i32, i32, i64) {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    let spg_storage::Value::Interval {
        months,
        days,
        micros,
    } = rows[0].values[0]
    else {
        panic!("expected Interval, got {:?}", rows[0].values[0]);
    };
    (months, days, micros)
}

#[test]
fn multiply_both_orders() {
    let mut e = Engine::new();
    assert_eq!(
        interval(&mut e, "SELECT INTERVAL '2 hours' * 3"),
        (0, 0, 6 * 3_600_000_000)
    );
    assert_eq!(
        interval(&mut e, "SELECT 3 * INTERVAL '2 hours'"),
        (0, 0, 6 * 3_600_000_000)
    );
    // Fractional day spills to micros: 1 day * 2.5 = 2 days 12:00:00.
    assert_eq!(
        interval(&mut e, "SELECT INTERVAL '1 day' * 2.5"),
        (0, 2, 12 * 3_600_000_000)
    );
    // Fractional month spills at 30 days/month: 1 month * 1.5 = 1 mon 15 days.
    assert_eq!(
        interval(&mut e, "SELECT INTERVAL '1 month' * 1.5"),
        (1, 15, 0)
    );
}

#[test]
fn divide() {
    let mut e = Engine::new();
    assert_eq!(
        interval(&mut e, "SELECT INTERVAL '4 hours' / 2"),
        (0, 0, 2 * 3_600_000_000)
    );
    // 1 day / 4 = 6:00:00 — day fraction spills down.
    assert_eq!(
        interval(&mut e, "SELECT INTERVAL '1 day' / 4"),
        (0, 0, 6 * 3_600_000_000)
    );
    assert!(e.execute("SELECT INTERVAL '1 hour' / 0").is_err());
}

#[test]
fn unary_minus() {
    let mut e = Engine::new();
    assert_eq!(
        interval(&mut e, "SELECT - INTERVAL '1 hour'"),
        (0, 0, -3_600_000_000)
    );
    assert_eq!(
        interval(&mut e, "SELECT - INTERVAL '1 mon 2 days'"),
        (-1, -2, 0)
    );
}
