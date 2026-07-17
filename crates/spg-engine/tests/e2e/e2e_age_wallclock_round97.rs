//! v7.39 (read01 round 97) — single-arg `age(t)` uses the real clock.
//!
//! PG's `age(t)` is `age(date_trunc('day', current_timestamp), t)` — the age
//! of `t` relative to midnight today. SPG's eval fallback anchors at
//! 2020-01-01 (the embedded engine has no wall clock), and NOTHING upgraded it
//! when a clock WAS set, so `age(ts)` over the wire returned a fixed
//! 2020-anchored interval instead of the real one. The clock rewrite now
//! injects today's midnight as an explicit first argument when a clock exists.
//!
//! Values locked against live PG 18.4 with the same "today" (2024-06-15).

use spg_engine::{Engine, QueryResult};

/// 2024-06-15 00:00:00 UTC in micros since the Unix epoch — a fixed "today"
/// so the wall-clock age is deterministic.
fn fixed_today() -> i64 {
    1_718_409_600_000_000
}

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn single_arg_age_uses_the_wall_clock() {
    let mut e = Engine::new().with_clock(fixed_today);
    // age(date) — relative to midnight today (2024-06-15).
    assert_eq!(
        text(&mut e, "SELECT age(date '2024-03-15')::text"),
        "3 mons"
    );
    // age(timestamp) — keeps the sub-day remainder.
    assert_eq!(
        text(&mut e, "SELECT age(timestamp '2023-06-15 06:00')::text"),
        "11 mons 29 days 18:00:00"
    );
    // A future date gives a negative age.
    assert_eq!(
        text(&mut e, "SELECT age(date '2024-07-20')::text"),
        "-1 mons -5 days"
    );
}

#[test]
fn age_xid_overload_untouched() {
    // The integer (xid) overload must NOT get a clock argument injected.
    let mut e = Engine::new().with_clock(fixed_today);
    assert_eq!(text(&mut e, "SELECT age(12345)::text"), "0");
}

#[test]
fn no_clock_keeps_deterministic_2020_anchor() {
    // Without a clock the eval fallback still anchors at 2020-01-01, so the
    // embedded no-clock behaviour (and its tests) is unchanged.
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT age(date '2020-01-01')::text"),
        "00:00:00"
    );
    assert_eq!(text(&mut e, "SELECT age(date '2019-12-01')::text"), "1 mon");
}
