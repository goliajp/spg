//! v7.17.0 Phase 3.P0-29 — MySQL time aliases.
//!
//! Reference:
//!   https://dev.mysql.com/doc/refman/8.0/en/date-and-time-functions.html
//!
//! Surface covered:
//!   * `date_format(t, fmt)` — MySQL-style format tokens (`%Y`, `%m`,
//!     `%d`, `%H`, `%i`, `%s`, `%f`, `%p`, `%M`, `%b`, `%%`).
//!   * `unix_timestamp()` — current epoch seconds (BIGINT). Folded
//!     at the clock-rewrite layer (same path as `now()`).
//!   * `unix_timestamp(t)` — converts a TIMESTAMP / DATE to epoch
//!     seconds (BIGINT).
//!   * `from_unixtime(n)` — epoch seconds → TIMESTAMP.
//!   * `from_unixtime(n, fmt)` — same, then date_format applied →
//!     TEXT.
//!
//! Invariants pinned:
//!   * date_format treats `%i` as MINUTE (NOT `%M`, which is month
//!     name in MySQL — easy footgun if we mirror PG's `to_char` tokens
//!     by accident).
//!   * date_format on NULL → NULL.
//!   * unix_timestamp(NULL) → NULL.
//!   * unix_timestamp on DATE = midnight UTC of that date.
//!   * from_unixtime(NULL) → NULL.
//!
//! WordPress / Laravel / mysql-connector-python apps emit
//! `date_format(created_at, '%Y-%m-%d %H:%i:%s')` for display, and
//! `unix_timestamp(created_at)` for cache keys; both are constant
//! in real query traffic.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn one_row(r: QueryResult) -> Vec<Value> {
    match r {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap().values
        }
        _ => panic!("expected Rows"),
    }
}

fn one_cell(eng: &mut Engine, sql: &str) -> Value {
    let row = one_row(eng.execute(sql).unwrap());
    assert_eq!(row.len(), 1, "{sql}");
    row.into_iter().next().unwrap()
}

fn text_of(v: Value) -> String {
    match v {
        Value::Text(s) => s,
        other => panic!("expected Value::Text, got {other:?}"),
    }
}

fn bigint_of(v: Value) -> i64 {
    match v {
        Value::BigInt(n) => n,
        Value::Int(n) => i64::from(n),
        other => panic!("expected Value::BigInt, got {other:?}"),
    }
}

// ── date_format ──────────────────────────────────────────────────

#[test]
fn date_format_iso_date_layout() {
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%Y-%m-%d')",
    ));
    assert_eq!(s, "2025-06-08");
}

#[test]
fn date_format_iso_datetime_layout() {
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%Y-%m-%d %H:%i:%s')",
    ));
    assert_eq!(s, "2025-06-08 14:30:45");
}

#[test]
fn date_format_percent_i_is_minute_not_month() {
    // MySQL: %i = minute (00-59). Easy footgun if we copy PG's %M.
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%i')",
    ));
    assert_eq!(s, "30");
}

#[test]
fn date_format_percent_capital_m_is_month_name() {
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%M')",
    ));
    assert_eq!(s, "June");
}

#[test]
fn date_format_percent_b_is_abbrev_month() {
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%b')",
    ));
    assert_eq!(s, "Jun");
}

#[test]
fn date_format_percent_p_renders_ampm() {
    let mut e = Engine::new();
    let am = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 09:30:45'::TIMESTAMP, '%p')",
    ));
    assert_eq!(am, "AM");
    let pm = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%p')",
    ));
    assert_eq!(pm, "PM");
}

#[test]
fn date_format_percent_f_microsecond_zero_pad_6() {
    let mut e = Engine::new();
    // SPG's TIMESTAMP literal parses ms truncated; supply via
    // arithmetic on epoch microseconds.
    // Test with the literal '2025-06-08 14:30:45.123456' if parsed.
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45.123456'::TIMESTAMP, '%f')",
    ));
    assert_eq!(s, "123456");
}

#[test]
fn date_format_double_percent_emits_literal_percent() {
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT date_format('2025-06-08 14:30:45'::TIMESTAMP, '%Y%%')",
    ));
    assert_eq!(s, "2025%");
}

#[test]
fn date_format_null_input_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        one_cell(&mut e, "SELECT date_format(NULL, '%Y')"),
        Value::Null
    ));
}

// ── unix_timestamp ───────────────────────────────────────────────

#[test]
fn unix_timestamp_of_known_timestamp() {
    let mut e = Engine::new();
    // 2025-06-08 14:30:45 UTC = 1749393045 seconds since epoch.
    let n = bigint_of(one_cell(
        &mut e,
        "SELECT unix_timestamp('2025-06-08 14:30:45'::TIMESTAMP)",
    ));
    assert_eq!(n, 1_749_393_045);
}

#[test]
fn unix_timestamp_of_date_is_midnight_utc() {
    let mut e = Engine::new();
    // 2025-06-08 00:00:00 UTC = 1749340800.
    let n = bigint_of(one_cell(
        &mut e,
        "SELECT unix_timestamp('2025-06-08'::DATE)",
    ));
    assert_eq!(n, 1_749_340_800);
}

#[test]
fn unix_timestamp_null_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        one_cell(&mut e, "SELECT unix_timestamp(NULL)"),
        Value::Null
    ));
}

#[test]
fn unix_timestamp_bare_call_returns_bigint() {
    // Bare unix_timestamp() folds via clock_replacement_for to a
    // BigInt literal at rewrite time. Pin a deterministic clock so
    // the assertion is exact.
    //
    // 2025-06-08 14:30:45 UTC = 1_749_393_045 sec
    //                          = 1_749_393_045_000_000 µsec
    const FROZEN_NOW_US: i64 = 1_749_393_045_000_000;
    fn frozen_clock() -> i64 {
        FROZEN_NOW_US
    }
    let mut e = Engine::new().with_clock(frozen_clock);
    let n = bigint_of(one_cell(&mut e, "SELECT unix_timestamp()"));
    assert_eq!(n, 1_749_393_045);
}

// ── from_unixtime ────────────────────────────────────────────────

#[test]
fn from_unixtime_seconds_to_timestamp() {
    let mut e = Engine::new();
    let v = one_cell(&mut e, "SELECT from_unixtime(1749393045)");
    let Value::Timestamp(t) = v else {
        panic!("expected Timestamp, got {v:?}");
    };
    assert_eq!(t, 1_749_393_045 * 1_000_000);
}

#[test]
fn from_unixtime_two_arg_formats_text() {
    let mut e = Engine::new();
    let s = text_of(one_cell(
        &mut e,
        "SELECT from_unixtime(1749393045, '%Y-%m-%d %H:%i:%s')",
    ));
    assert_eq!(s, "2025-06-08 14:30:45");
}

#[test]
fn from_unixtime_null_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        one_cell(&mut e, "SELECT from_unixtime(NULL)"),
        Value::Null
    ));
}
