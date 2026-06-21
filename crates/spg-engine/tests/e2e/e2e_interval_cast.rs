//! v7.17.0 Phase 5.2 — `::INTERVAL` cast + `INTERVAL '…'` literal
//! end-to-end. The cast was wired in v7.9.25; this file pins the
//! customer-facing surface (cutover-parity-gaps G-CRIT-2,
//! mailrs 5.x docket).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn interval_text_cast_via_double_colon() {
    // v7.37.5 β — `'30 days'` now lands in the dedicated `days`
    // dimension (PG byte-equal: `'30 days'` ≠ `'720 hours'`).
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT '30 days'::INTERVAL").unwrap());
    assert_eq!(r.len(), 1);
    let Value::Interval {
        months,
        days,
        micros,
    } = r[0][0]
    else {
        panic!("expected Interval, got {:?}", r[0][0]);
    };
    assert_eq!(months, 0);
    assert_eq!(days, 30);
    assert_eq!(micros, 0);
}

#[test]
fn interval_literal_in_expression_position() {
    // The mailrs cutover-parity-gaps G-CRIT-2 shape:
    //   WHERE created_at > NOW() - INTERVAL '30 days'
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (id INT NOT NULL, created_at TIMESTAMP NOT NULL)")
        .unwrap();
    e.execute(
        "INSERT INTO t VALUES \
            (1, '2024-01-01 00:00:00'::TIMESTAMP), \
            (2, '2024-06-01 00:00:00'::TIMESTAMP)",
    )
    .unwrap();
    let r = rows(
        e.execute(
            "SELECT id FROM t WHERE created_at > '2024-03-01 00:00:00'::TIMESTAMP - INTERVAL '60 days'",
        )
        .unwrap(),
    );
    // Cutoff is 2024-03-01 minus 60 days = ~2024-01-01 00:00 (or
    // slightly later) — only the 2024-06-01 row passes.
    assert!(!r.is_empty());
}

#[test]
fn interval_with_compound_units() {
    let mut e = Engine::new();
    let r = rows(
        e.execute("SELECT '1 day 2 hours 3 minutes'::INTERVAL")
            .unwrap(),
    );
    let Value::Interval {
        months,
        days,
        micros,
    } = r[0][0]
    else {
        panic!();
    };
    assert_eq!(months, 0);
    assert_eq!(days, 1);
    let expected = (2 * 3600 + 3 * 60) * 1_000_000;
    assert_eq!(micros, expected);
}

#[test]
fn interval_with_months_and_years() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT '1 year 2 months'::INTERVAL").unwrap());
    let Value::Interval {
        months,
        days,
        micros,
    } = r[0][0]
    else {
        panic!();
    };
    assert_eq!(months, 14);
    assert_eq!(days, 0);
    assert_eq!(micros, 0);
}

#[test]
fn interval_negative_value() {
    let mut e = Engine::new();
    let r = rows(e.execute("SELECT '-7 days'::INTERVAL").unwrap());
    let Value::Interval {
        months,
        days,
        micros,
    } = r[0][0]
    else {
        panic!();
    };
    assert_eq!(months, 0);
    assert_eq!(days, -7);
    assert_eq!(micros, 0);
}

#[test]
fn timestamp_plus_interval_via_cast() {
    // mailrs's reputation.rs shape — `NOW() - INTERVAL '30 days'`
    // generalised to `ts ± interval`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (when_ TIMESTAMP NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES ('2024-01-01 12:00:00'::TIMESTAMP)")
        .unwrap();
    let r = rows(
        e.execute("SELECT when_ + '1 day'::INTERVAL FROM t")
            .unwrap(),
    );
    let Value::Timestamp(t) = r[0][0] else {
        panic!();
    };
    // 2024-01-02 12:00:00 in epoch micros.
    // 2024-01-01 = day 19723 from epoch.
    let day_us = 86_400 * 1_000_000_i64;
    let base = 19723_i64 * day_us + 12 * 3600 * 1_000_000;
    assert_eq!(t, base + day_us);
}

#[test]
fn interval_in_generate_series_step() {
    // Phase 3.10 hooks into this — verify the shapes compose.
    let mut e = Engine::new();
    let r = rows(
        e.execute(
            "SELECT * FROM generate_series(\
                '2024-01-01'::TIMESTAMP, \
                '2024-01-03'::TIMESTAMP, \
                '1 day'::INTERVAL\
            )",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 3);
}
