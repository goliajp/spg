//! v7.39.11 — the index-ordered walk takes LIMIT and OFFSET.
//!
//! `iter_desc`'s own doc calls this "the ORDER BY <indexed col> DESC +
//! LIMIT N executor path" — the shape the walk was written for — and
//! the streaming gate refused every statement that carried a LIMIT. So
//! a wire client asking for the newest twenty rows fell back to
//! building the whole answer and throwing all but twenty away.
//!
//! Reported by sentori against 7.39.10 as the third instance of
//! "indexes that were charging you and doing nothing", with the first
//! performance measurement either of us has taken on their shape:
//! `Limit -> Index Scan` on PostgreSQL 18 against `Limit -> Sort ->
//! Seq Scan` here.
//!
//! These pins go through `execute_readonly_select_streaming`, which is
//! the route an autocommit SELECT takes over the wire and the one the
//! walk lives on; `Engine::execute` materialises and reaches a
//! different top-N path. Every expectation is the answer the same
//! statement gives without the LIMIT, truncated — an ordering is only
//! served correctly if the bounded form agrees with the unbounded one.

use spg_engine::{CancelToken, Engine, StreamItem};
use spg_storage::Value;

fn streamed(e: &Engine, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(sql, CancelToken::none(), |item| {
        if let StreamItem::Row(cells) = item {
            out.push(match cells.get(0).expect("row has a first cell") {
                Value::Int(n) => n.to_string(),
                Value::Null => "NULL".to_string(),
                other => panic!("unexpected cell {other:?}"),
            });
        }
        Ok(())
    })
    .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    out
}

/// Ten rows, indexed, NOT NULL — the plain shape.
fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (k int NOT NULL, pad text)")
        .unwrap();
    for i in 1..=10 {
        e.execute(&format!("INSERT INTO w VALUES ({i}, 'x')"))
            .unwrap();
    }
    e.execute("CREATE INDEX w_k ON w (k)").unwrap();
    e
}

#[test]
fn a_limit_takes_the_first_n_of_the_unbounded_answer() {
    let e = seeded();
    let all = streamed(&e, "SELECT k FROM w ORDER BY k");
    assert_eq!(all.len(), 10);
    assert_eq!(streamed(&e, "SELECT k FROM w ORDER BY k LIMIT 3"), all[..3]);
}

#[test]
fn descending_too() {
    let e = seeded();
    let all = streamed(&e, "SELECT k FROM w ORDER BY k DESC");
    assert_eq!(
        streamed(&e, "SELECT k FROM w ORDER BY k DESC LIMIT 4"),
        all[..4]
    );
}

#[test]
fn an_offset_skips_and_the_limit_still_counts_from_there() {
    let e = seeded();
    let all = streamed(&e, "SELECT k FROM w ORDER BY k");
    assert_eq!(
        streamed(&e, "SELECT k FROM w ORDER BY k OFFSET 4 LIMIT 3"),
        all[4..7]
    );
    assert_eq!(
        streamed(&e, "SELECT k FROM w ORDER BY k OFFSET 8"),
        all[8..]
    );
}

#[test]
fn a_limit_past_the_end_is_the_whole_answer() {
    let e = seeded();
    let all = streamed(&e, "SELECT k FROM w ORDER BY k");
    assert_eq!(streamed(&e, "SELECT k FROM w ORDER BY k LIMIT 100"), all);
    assert_eq!(
        streamed(&e, "SELECT k FROM w ORDER BY k OFFSET 100"),
        Vec::<String>::new()
    );
}

#[test]
fn offset_and_limit_count_rows_that_pass_the_predicate() {
    // A skipped row still has to run the WHERE — OFFSET counts rows
    // that PASS, not rows the walk stepped over.
    let e = seeded();
    let all = streamed(&e, "SELECT k FROM w WHERE k % 2 = 0 ORDER BY k");
    assert_eq!(all, ["2", "4", "6", "8", "10"]);
    assert_eq!(
        streamed(
            &e,
            "SELECT k FROM w WHERE k % 2 = 0 ORDER BY k OFFSET 1 LIMIT 2"
        ),
        ["4", "6"]
    );
}

#[test]
fn the_null_rows_are_counted_by_the_limit_too() {
    // A nullable key emits its NULL rows in a separate pass, at the end
    // SQL puts them, and those rows are part of the answer the LIMIT
    // bounds. Ascending puts them last; `DESC NULLS LAST` is the walk's
    // other recovered case.
    let mut e = Engine::new();
    e.execute("CREATE TABLE n (k int)").unwrap();
    for v in ["1", "2", "NULL", "3", "NULL"] {
        e.execute(&format!("INSERT INTO n VALUES ({v})")).unwrap();
    }
    e.execute("CREATE INDEX n_k ON n (k)").unwrap();
    let all = streamed(&e, "SELECT k FROM n ORDER BY k");
    assert_eq!(all, ["1", "2", "3", "NULL", "NULL"]);
    assert_eq!(streamed(&e, "SELECT k FROM n ORDER BY k LIMIT 4"), all[..4]);
    assert_eq!(
        streamed(&e, "SELECT k FROM n ORDER BY k OFFSET 3 LIMIT 1"),
        all[3..4]
    );
}

/// The witness that the walk actually STOPS, rather than producing the
/// right rows by some other route.
///
/// The pins above would pass on the fallback too — the same rows, more
/// slowly — so they say nothing about early stopping. This one does:
/// the projection divides by zero on the LAST row in key order, so an
/// executor that evaluates every row raises and one that stops after
/// three does not. PostgreSQL 18.6 answers `1 2 3` here for the same
/// reason.
#[test]
fn the_walk_stops_before_the_row_that_would_raise() {
    let e = seeded();
    let mut out: Vec<String> = Vec::new();
    e.execute_readonly_select_streaming(
        "SELECT 30 / (k - 10) FROM w ORDER BY k LIMIT 3",
        CancelToken::none(),
        |item| {
            if let StreamItem::Row(cells) = item
                && let Value::Int(n) = cells.get(0).expect("first cell")
            {
                out.push(n.to_string());
            }
            Ok(())
        },
    )
    .expect("stopping before k = 10 means never dividing by zero");
    assert_eq!(out.len(), 3);
    // And the row IS there to be reached: without the LIMIT it raises.
    assert!(
        e.execute_readonly_select_streaming(
            "SELECT 30 / (k - 10) FROM w ORDER BY k",
            CancelToken::none(),
            |_| Ok(()),
        )
        .is_err(),
        "the control: the offending row must be reachable, or the pin \
         above proves nothing"
    );
}
