//! Window-function PG18 differential corpus (7th differential sweep).
//!
//! Ground truth captured from live PostgreSQL 18.4 on 2026-07-04 over a
//! small seeded table `w(id, g, x, y numeric(10,2), s)` with NULLs, ties,
//! multiple groups, a single-row group, and a numeric column. Each row of
//! every result set (ordered by id) is rendered and joined with `|`; the
//! expected string is exactly what PG produced (except four documented
//! representation / deferred divergences, called out inline).
//!
//! BUGS FIXED in the accompanying commit and locked here:
//!   * min/max OVER on TEXT / NUMERIC / DATE returned NULL — the window
//!     aggregate path forced every value through `value_to_f64` (None for
//!     non-f64 types), so `max(text_col) OVER (...)` / `min(numeric_col)
//!     OVER (...)` silently produced NULL. Now compared via `value_cmp`
//!     on the actual Value, preserving type (see `min_max_text_numeric`).
//!   * sum OVER an empty / all-NULL frame returned 0 — `Value::Float(sum)`
//!     was emitted even when no non-NULL value contributed. PG returns
//!     NULL. Now `count == 0 => NULL` (see `sum_all_null_partition`).
//!   * ROWS frame lying entirely outside the partition (e.g. `ROWS BETWEEN
//!     2 PRECEDING AND 1 PRECEDING` on the first row, or `1 FOLLOWING AND
//!     2 FOLLOWING` on the last) pulled the boundary row into the frame —
//!     saturating_sub / .min(last) collapsed the out-of-range bounds onto
//!     index 0 / last. PG treats it as an empty frame. Now recognised as
//!     empty (see `rows_frame_out_of_range`).
//!
//! DOCUMENTED (not bugs / deferred):
//!   * avg(int/float) OVER returns FLOAT; PG returns NUMERIC (prints
//!     trailing zeros). Same value.
//!
//! CLOSED GAP (v7.37.16 — was DEFERRED):
//!   * sum(NUMERIC) / avg(NUMERIC) OVER previously returned NULL (no
//!     exact accumulator). Now they use the exact i128-mantissa
//!     accumulator + PG's division display scale for avg, matching PG18's
//!     exact numeric running sum / partition avg (see
//!     `numeric_agg_exact`).

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn render(v: &Value) -> String {
    match v {
        Value::Null => "<NULL>".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Text(s) => s.to_string(),
        Value::Numeric { scaled, scale, .. } => {
            let neg = *scaled < 0;
            let mut digits = scaled.unsigned_abs().to_string();
            let sc = *scale as usize;
            if sc > 0 {
                while digits.len() <= sc {
                    digits.insert(0, '0');
                }
                digits.insert(digits.len() - sc, '.');
            }
            if neg { format!("-{digits}") } else { digits }
        }
        other => format!("{other:?}"),
    }
}

fn seed() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id int, g text, x int, y numeric(10,2), s text)")
        .unwrap();
    for row in [
        "(1,'a',10, 1.5,  'foo')",
        "(2,'a',20, 2.5,  'bar')",
        "(3,'a',20, 2.5,  'bar')",
        "(4,'a',NULL,NULL, NULL)",
        "(5,'b',30, 9.99, 'baz')",
        "(6,'b',40, 12.50,'qux')",
        "(7,'c',50, 5.25, 'zap')",
    ] {
        e.execute(&format!("INSERT INTO w VALUES {row}")).unwrap();
    }
    e
}

/// Run `SELECT id, <expr> AS v FROM w ORDER BY id`, render column v of
/// every row, join with `|`.
fn col(e: &mut Engine, expr: &str) -> String {
    let sql = format!("SELECT id, {expr} AS v FROM w ORDER BY id");
    match e.execute(&sql) {
        Ok(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|r| render(&r.values[r.values.len() - 1]))
            .collect::<Vec<_>>()
            .join("|"),
        Ok(other) => format!("<NONROWS:{other:?}>"),
        Err(er) => format!("<ERR:{er:?}>"),
    }
}

fn ck(e: &mut Engine, expr: &str, want: &str) {
    let got = col(e, expr);
    assert_eq!(got, want, "\n  expr: {expr}\n  want: {want}\n  got:  {got}");
}

#[test]
fn ranking() {
    let mut e = seed();
    ck(&mut e, "row_number() OVER (ORDER BY x)", "1|2|3|7|4|5|6");
    ck(
        &mut e,
        "row_number() OVER (PARTITION BY g ORDER BY x)",
        "1|2|3|4|1|2|1",
    );
    ck(&mut e, "rank() OVER (ORDER BY x)", "1|2|2|7|4|5|6");
    ck(
        &mut e,
        "rank() OVER (PARTITION BY g ORDER BY x)",
        "1|2|2|4|1|2|1",
    );
    ck(&mut e, "dense_rank() OVER (ORDER BY x)", "1|2|2|6|3|4|5");
    ck(
        &mut e,
        "dense_rank() OVER (PARTITION BY g ORDER BY x)",
        "1|2|2|3|1|2|1",
    );
    ck(
        &mut e,
        "percent_rank() OVER (PARTITION BY g ORDER BY x)",
        "0|0.3333333333333333|0.3333333333333333|1|0|1|0",
    );
    ck(
        &mut e,
        "cume_dist() OVER (PARTITION BY g ORDER BY x)",
        "0.25|0.75|0.75|1|0.5|1|1",
    );
    ck(&mut e, "ntile(3) OVER (ORDER BY id)", "1|1|1|2|2|3|3");
    ck(
        &mut e,
        "ntile(2) OVER (PARTITION BY g ORDER BY id)",
        "1|1|2|2|1|2|1",
    );
    ck(&mut e, "rank() OVER (ORDER BY y)", "1|2|2|7|5|6|4");
    ck(
        &mut e,
        "row_number() OVER (ORDER BY y DESC)",
        "7|5|6|1|3|2|4",
    );
    ck(&mut e, "rank() OVER (ORDER BY g, x)", "1|2|2|4|5|6|7");
    ck(
        &mut e,
        "row_number() OVER (ORDER BY x DESC, id)",
        "7|5|6|1|4|3|2",
    );
}

#[test]
fn offset() {
    let mut e = seed();
    ck(
        &mut e,
        "lag(x) OVER (ORDER BY id)",
        "<NULL>|10|20|20|<NULL>|30|40",
    );
    ck(
        &mut e,
        "lag(x,2) OVER (ORDER BY id)",
        "<NULL>|<NULL>|10|20|20|<NULL>|30",
    );
    ck(
        &mut e,
        "lag(x,2,-1) OVER (ORDER BY id)",
        "-1|-1|10|20|20|<NULL>|30",
    );
    ck(
        &mut e,
        "lead(x) OVER (ORDER BY id)",
        "20|20|<NULL>|30|40|50|<NULL>",
    );
    ck(
        &mut e,
        "lead(x,1,0) OVER (ORDER BY id)",
        "20|20|<NULL>|30|40|50|0",
    );
    ck(
        &mut e,
        "lag(x) OVER (PARTITION BY g ORDER BY id)",
        "<NULL>|10|20|20|<NULL>|30|<NULL>",
    );
    ck(
        &mut e,
        "lead(x) OVER (PARTITION BY g ORDER BY id)",
        "20|20|<NULL>|<NULL>|40|<NULL>|<NULL>",
    );
    ck(
        &mut e,
        "lag(x) OVER (ORDER BY x NULLS FIRST)",
        "<NULL>|10|20|<NULL>|20|30|40",
    );
}

#[test]
fn value() {
    let mut e = seed();
    ck(
        &mut e,
        "first_value(x) OVER (PARTITION BY g ORDER BY id)",
        "10|10|10|10|30|30|50",
    );
    ck(
        &mut e,
        "last_value(x) OVER (PARTITION BY g ORDER BY id)",
        "10|20|20|<NULL>|30|40|50",
    );
    ck(
        &mut e,
        "last_value(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
        "<NULL>|<NULL>|<NULL>|<NULL>|40|40|50",
    );
    ck(
        &mut e,
        "nth_value(x,2) OVER (PARTITION BY g ORDER BY id)",
        "<NULL>|20|20|20|<NULL>|40|<NULL>",
    );
    ck(
        &mut e,
        "first_value(x) OVER (ORDER BY id)",
        "10|10|10|10|10|10|10",
    );
}

#[test]
fn frames() {
    let mut e = seed();
    ck(
        &mut e,
        "sum(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)",
        "30|50|40|50|70|120|90",
    );
    ck(
        &mut e,
        "sum(x) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING)",
        "10|30|50|50|80|120|170",
    );
    ck(
        &mut e,
        "sum(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
        "50|50|50|50|70|70|50",
    );
    ck(
        &mut e,
        "sum(x) OVER (ORDER BY x)",
        "10|50|50|170|80|120|170",
    );
    // v7.38 (read01) — avg over int inputs is NUMERIC in a window too (as for
    // GROUP BY avg). Values are live-PG18.4-exact.
    ck(
        &mut e,
        "avg(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)",
        "15.0000000000000000|16.6666666666666667|20.0000000000000000|25.0000000000000000|35.0000000000000000|40.0000000000000000|45.0000000000000000",
    );
    ck(&mut e, "count(*) OVER (ORDER BY x)", "1|3|3|7|4|5|6");
    ck(
        &mut e,
        "sum(x) OVER (ORDER BY x RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
        "170|170|170|170|170|170|170",
    );
    ck(
        &mut e,
        "max(x) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING)",
        "20|20|20|30|40|50|50",
    );
}

/// `RANGE BETWEEN <interval> PRECEDING/FOLLOWING` — value-based frame over a
/// DATE / TIMESTAMP ORDER BY column (PG time-series window). All values are
/// live-PG18.4-verified; the outer query orders deterministically so the
/// per-row window results line up.
#[test]
fn range_interval_offset_frame() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE wf(d date, ts timestamp, v int)")
        .unwrap();
    for row in [
        "('2020-01-01','2020-01-01 10:00',1)",
        "('2020-01-02','2020-01-01 11:00',2)",
        "('2020-01-02','2020-01-01 12:30',3)",
        "('2020-01-05','2020-01-01 13:00',4)",
    ] {
        e.execute(&format!("INSERT INTO wf VALUES {row}")).unwrap();
    }
    let q = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql) {
            Ok(QueryResult::Rows { rows, .. }) => rows
                .iter()
                .map(|r| render(&r.values[r.values.len() - 1]))
                .collect::<Vec<_>>()
                .join("|"),
            Ok(o) => format!("<NONROWS:{o:?}>"),
            Err(er) => format!("<ERR:{er:?}>"),
        }
    };
    // DATE key, 1-day PRECEDING window (peers within a day collapse).
    assert_eq!(
        q(
            &mut e,
            "SELECT count(*) OVER (ORDER BY d RANGE BETWEEN '1 day'::interval PRECEDING AND CURRENT ROW) FROM wf ORDER BY d, v"
        ),
        "1|3|3|1"
    );
    // TIMESTAMP key, 2-hour PRECEDING sliding sum.
    assert_eq!(
        q(
            &mut e,
            "SELECT sum(v) OVER (ORDER BY ts RANGE BETWEEN interval '2 hours' PRECEDING AND CURRENT ROW) FROM wf ORDER BY ts"
        ),
        "1|3|5|9"
    );
    // Calendar-aware 1-month FOLLOWING window.
    assert_eq!(
        q(
            &mut e,
            "SELECT count(*) OVER (ORDER BY d RANGE BETWEEN CURRENT ROW AND '1 month'::interval FOLLOWING) FROM wf ORDER BY d, v"
        ),
        "4|3|3|1"
    );
    // DESC ordering flips the offset direction; results emitted in d,v order.
    assert_eq!(
        q(
            &mut e,
            "SELECT count(*) OVER (ORDER BY d DESC RANGE BETWEEN '1 day'::interval PRECEDING AND CURRENT ROW) FROM wf ORDER BY d, v"
        ),
        "3|2|2|1"
    );
    // Honest errors: an INTERVAL offset is RANGE-only, and RANGE INTERVAL
    // needs a temporal ORDER BY key.
    assert!(e.execute("SELECT count(*) OVER (ORDER BY v ROWS BETWEEN '1 day'::interval PRECEDING AND CURRENT ROW) FROM wf").is_err());
    assert!(e.execute("SELECT count(*) OVER (ORDER BY v RANGE BETWEEN '1 day'::interval PRECEDING AND CURRENT ROW) FROM wf").is_err());
}

#[test]
fn aggregates_as_window() {
    let mut e = seed();
    ck(
        &mut e,
        "sum(x) OVER (ORDER BY id)",
        "10|30|50|50|80|120|170",
    );
    ck(
        &mut e,
        "avg(x) OVER (PARTITION BY g)",
        "16.6666666666666667|16.6666666666666667|16.6666666666666667|16.6666666666666667|35.0000000000000000|35.0000000000000000|50.0000000000000000",
    ); // live-PG18.4-exact numeric
    ck(&mut e, "count(*) OVER ()", "7|7|7|7|7|7|7");
    ck(
        &mut e,
        "max(x) OVER (PARTITION BY g)",
        "20|20|20|20|40|40|50",
    );
    ck(
        &mut e,
        "min(x) OVER (PARTITION BY g)",
        "10|10|10|10|30|30|50",
    );
    ck(&mut e, "count(x) OVER (PARTITION BY g)", "3|3|3|3|2|2|1");
    ck(&mut e, "count(*) OVER (PARTITION BY g)", "4|4|4|4|2|2|1");
    ck(
        &mut e,
        "sum(x) OVER (PARTITION BY s)",
        "10|40|40|<NULL>|30|40|50",
    ); // FIX: sum(x) OVER all-NULL partition was 0; now NULL (PG).
}

#[test]
fn ordering_nulls() {
    let mut e = seed();
    ck(
        &mut e,
        "row_number() OVER (ORDER BY x NULLS FIRST)",
        "2|3|4|1|5|6|7",
    );
    ck(
        &mut e,
        "row_number() OVER (ORDER BY x NULLS LAST)",
        "1|2|3|7|4|5|6",
    );
    ck(
        &mut e,
        "rank() OVER (ORDER BY x DESC NULLS LAST)",
        "6|4|4|7|3|2|1",
    );
}

#[test]
fn min_max_text_numeric() {
    let mut e = seed();
    ck(
        &mut e,
        "max(s) OVER (PARTITION BY g)",
        "foo|foo|foo|foo|qux|qux|zap",
    ); // FIX: max(TEXT) OVER was NULL (f64-only path); now lexical max.
    ck(
        &mut e,
        "min(s) OVER (PARTITION BY g)",
        "bar|bar|bar|bar|baz|baz|zap",
    ); // FIX: min(TEXT) OVER was NULL; now lexical min.
    ck(
        &mut e,
        "max(y) OVER (PARTITION BY g)",
        "2.50|2.50|2.50|2.50|12.50|12.50|5.25",
    ); // FIX: max(NUMERIC) OVER was NULL; now exact numeric max.
    ck(
        &mut e,
        "min(y) OVER (PARTITION BY g)",
        "1.50|1.50|1.50|1.50|9.99|9.99|5.25",
    ); // FIX: min(NUMERIC) OVER was NULL; now exact numeric min.
}

// CLOSED GAP (v7.37.16): sum/avg(NUMERIC) OVER now exact, PG18-matched.
#[test]
fn numeric_agg_exact() {
    let mut e = seed();
    // Exact running numeric sum (was deferred → all NULL).
    ck(
        &mut e,
        "sum(y) OVER (ORDER BY id)",
        "1.50|4.00|6.50|6.50|16.49|28.99|34.24",
    );
    // Exact numeric partition avg at PG's division display scale display scale
    // (16 fractional digits here). 'a' 6.50/3, 'b' 22.49/2, 'c' 5.25/1.
    ck(
        &mut e,
        "avg(y) OVER (PARTITION BY g)",
        "2.1666666666666667|2.1666666666666667|2.1666666666666667|2.1666666666666667|11.2450000000000000|11.2450000000000000|5.2500000000000000",
    );
}

#[test]
fn sum_all_null_partition() {
    let mut e = seed();
    ck(
        &mut e,
        "sum(x) OVER (PARTITION BY s)",
        "10|40|40|<NULL>|30|40|50",
    ); // FIX: sum(x) OVER all-NULL partition was 0; now NULL (PG).
}

#[test]
fn rows_frame_out_of_range() {
    let mut e = seed();
    ck(
        &mut e,
        "sum(x) OVER (PARTITION BY g ORDER BY id ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING)",
        "<NULL>|10|30|40|<NULL>|30|<NULL>",
    ); // FIX: ROWS frame fully before partition start was current-row; now empty->NULL (PG).
}
