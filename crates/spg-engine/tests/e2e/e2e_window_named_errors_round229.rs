//! v7.39 (round 229) — window-clause differential against live PG18.4.
//! The frame semantics themselves (ROWS / RANGE / GROUPS × all four
//! EXCLUDE modes, the rank family, lag/lead/nth_value, FILTER) already
//! matched row-for-row; this round closes what the sweep found missing:
//!
//!   * `OVER (w1 …)` — copying a named window, with PG's three refusal
//!     cases (override PARTITION BY / override ORDER BY / copy a base
//!     that has a frame) and duplicate `WINDOW` names;
//!   * window calls in WHERE / HAVING, which used to reach row eval and
//!     surface an internal "engine rewrite bug" message;
//!   * three frames whose start is after their end, which SPG silently
//!     answered with an all-NULL column instead of rejecting.

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id int, g text, v int)").unwrap();
    e.execute("INSERT INTO w VALUES (1,'a',10),(2,'a',20),(3,'a',20),(4,'b',5),(5,'b',15),(6,'b',15),(7,'b',30)")
        .unwrap();
    e
}

fn col1(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match &r.values[r.values.len() - 1] {
                spg_storage::Value::Null => String::new(),
                other => format!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(err) => format!("{err}"),
        Ok(ok) => panic!("expected an error, got {ok:?}"),
    }
}

#[test]
fn named_window_copy_inherits_partitioning() {
    let mut e = seeded();
    // `w2 AS (w1 ORDER BY v)` and `OVER (w1 ORDER BY v)` both take w1's
    // PARTITION BY and add the ordering. Values verified against PG18.4.
    let want = ["BigInt(10)", "BigInt(50)", "BigInt(50)", "BigInt(5)", "BigInt(35)", "BigInt(35)", "BigInt(65)"];
    assert_eq!(
        col1(
            &mut e,
            "SELECT id, sum(v) OVER w2 FROM w \
             WINDOW w1 AS (PARTITION BY g), w2 AS (w1 ORDER BY v) ORDER BY id"
        ),
        want
    );
    assert_eq!(
        col1(
            &mut e,
            "SELECT id, sum(v) OVER (w1 ORDER BY v) FROM w \
             WINDOW w1 AS (PARTITION BY g) ORDER BY id"
        ),
        want
    );
    // A copy may add a frame when the base has none.
    assert_eq!(
        col1(
            &mut e,
            "SELECT id, sum(v) OVER (w1 ROWS 1 PRECEDING) FROM w \
             WINDOW w1 AS (PARTITION BY g ORDER BY v) ORDER BY id"
        ),
        ["BigInt(10)", "BigInt(30)", "BigInt(40)", "BigInt(5)", "BigInt(20)", "BigInt(30)", "BigInt(45)"]
    );
}

#[test]
fn named_window_copy_refusals_match_pg() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "SELECT sum(v) OVER (w1 PARTITION BY g) FROM w WINDOW w1 AS (PARTITION BY g)",
            "cannot override PARTITION BY clause of window \"w1\"",
        ),
        (
            "SELECT sum(v) OVER (w1 ORDER BY v) FROM w WINDOW w1 AS (PARTITION BY g ORDER BY v)",
            "cannot override ORDER BY clause of window \"w1\"",
        ),
        (
            "SELECT sum(v) OVER (w1 ROWS 1 PRECEDING) FROM w WINDOW w1 AS (ORDER BY v ROWS 2 PRECEDING)",
            "cannot copy window \"w1\" because it has a frame clause",
        ),
        (
            "SELECT sum(v) OVER (w1) FROM w WINDOW w1 AS (ORDER BY v ROWS 2 PRECEDING)",
            "cannot copy window \"w1\" because it has a frame clause",
        ),
        (
            "SELECT sum(v) OVER w1 FROM w WINDOW w1 AS (PARTITION BY g), w1 AS (PARTITION BY g)",
            "window \"w1\" is already defined",
        ),
        (
            "SELECT sum(v) OVER nosuch FROM w",
            "window \"nosuch\" does not exist",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "want {want:?} in {got:?}");
    }
}

#[test]
fn window_calls_rejected_in_where_and_having() {
    let mut e = seeded();
    // Both clauses run before the window pass, so PG refuses outright.
    // SPG used to leak "window function reached row eval — engine rewrite
    // bug" from the evaluator here.
    let got = err(
        &mut e,
        "SELECT rank() OVER (ORDER BY v) FROM w WHERE rank() OVER (ORDER BY v) = 1",
    );
    assert!(got.contains("window functions are not allowed in WHERE"), "{got}");
    assert!(!got.contains("rewrite bug"), "no internal wording: {got}");
    let got = err(
        &mut e,
        "SELECT id FROM w GROUP BY id HAVING row_number() OVER () = 1",
    );
    assert!(got.contains("window functions are not allowed in HAVING"), "{got}");
}

#[test]
fn impossible_frames_are_rejected_not_silently_empty() {
    let mut e = seeded();
    for (sql, want) in [
        (
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW) FROM w",
            "frame start cannot be UNBOUNDED FOLLOWING",
        ),
        (
            "SELECT sum(v) OVER (ORDER BY v RANGE BETWEEN 1 PRECEDING AND UNBOUNDED PRECEDING) FROM w",
            "frame end cannot be UNBOUNDED PRECEDING",
        ),
        // These three answered with an all-NULL column before r229.
        (
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN CURRENT ROW AND 1 PRECEDING) FROM w",
            "frame starting from current row cannot have preceding rows",
        ),
        (
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW) FROM w",
            "frame starting from following row cannot have preceding rows",
        ),
        (
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN 1 FOLLOWING AND 1 PRECEDING) FROM w",
            "frame starting from following row cannot have preceding rows",
        ),
    ] {
        let got = err(&mut e, sql);
        assert!(got.contains(want), "want {want:?} in {got:?}");
    }
    // The legal neighbours of those frames still run.
    assert_eq!(
        col1(
            &mut e,
            "SELECT sum(v) OVER (ORDER BY v ROWS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) \
             FROM w ORDER BY 1"
        ),
        ["BigInt(30)", "BigInt(50)", "BigInt(70)", "BigInt(85)", "BigInt(100)", "BigInt(110)", "BigInt(115)"]
    );
}

#[test]
fn range_offset_refusals_name_pgs_two_cases() {
    let mut e = seeded();
    let got = err(
        &mut e,
        "SELECT sum(v) OVER (ORDER BY g RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
    );
    assert!(
        got.contains(
            "RANGE with offset PRECEDING/FOLLOWING is not supported for column type text"
        ),
        "{got}"
    );
    let got = err(
        &mut e,
        "SELECT sum(v) OVER (ORDER BY v, g RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM w",
    );
    assert!(
        got.contains(
            "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column"
        ),
        "{got}"
    );
}
