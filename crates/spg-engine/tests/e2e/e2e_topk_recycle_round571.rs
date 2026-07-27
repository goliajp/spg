//! v7.39 (round 571) — the top-N scan stops throwing its buffers away.
//!
//! Round 570 measured `ORDER BY … LIMIT k` flat in k — `LIMIT 1` cost
//! what `LIMIT 1000` cost, and three times what `max(id)` costs for the
//! same answer — and profiled a quarter of the connection thread into
//! the allocator. Per input row the scan builds TWO vectors: the
//! projected row's values and the `Vec<OrderKey>` beside it. The
//! accumulator is bounded at `2k` by `topk_trim`, so on 500k rows with
//! `LIMIT 10` it built half a million pairs and kept ten.
//!
//! Round 485 had already made the scan share one projection buffer, but
//! a surviving row takes it (`mem::take`) and without DISTINCT almost
//! every row survives — so the next row started from zero capacity and
//! allocated. The trim drops `k` rows at a time; their buffers come back
//! to a pool now, with the capacity they had already grown.
//!
//! Over pgwire, 500k rows, three paired batches, medians:
//!
//!                          before    after    PG18
//!     ORDER BY id DESC      68.25 ms  57.56    13.5     5.06x -> 4.26x
//!     … two projected cols  66.57     61.44    13.5
//!     … two order keys      75.01     65.24    15.2
//!
//! Lower in 3 of 3 batches on each. PG still leads by a lot — this takes
//! out the allocation, not the remaining per-row work.
//!
//! What the pins below are for: a recycled buffer that kept a stale
//! value would be a silent wrong answer, and only a scan long enough to
//! trim many times would show it. Each of these runs well past `2k` rows
//! so the pool turns over repeatedly.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn engine() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t571 (id INT, g INT, t TEXT)").unwrap();
    e.execute("INSERT INTO t571 SELECT gg, gg % 7, 'row' || gg FROM generate_series(1, 5000) gg")
        .unwrap();
    e
}

/// 5000 rows and `LIMIT 10` means the pool turns over ~250 times.
#[test]
fn round571_topk_answers_after_many_trims() {
    let mut e = engine();
    assert_eq!(
        vals(&mut e, "SELECT id FROM t571 ORDER BY id DESC LIMIT 10"),
        (4991..=5000).rev().map(|i| i.to_string()).collect::<Vec<_>>()
    );
    assert_eq!(
        vals(&mut e, "SELECT id FROM t571 ORDER BY id LIMIT 10"),
        (1..=10).map(|i| i.to_string()).collect::<Vec<_>>()
    );
    // A wider projection — every recycled buffer must hold exactly the
    // columns this row projected, not the previous row's.
    let want: Vec<String> = (4998..=5000)
        .rev()
        .map(|i: i32| format!("{i}|{}|row{i}", i % 7))
        .collect();
    assert_eq!(
        vals(&mut e, "SELECT id, g, t FROM t571 ORDER BY id DESC LIMIT 3"),
        want
    );
    // Two order keys, so the key buffer recycles too.
    // The largest g is 6; within it the largest ids, descending.
    let want: Vec<String> = (1..=5000)
        .rev()
        .filter(|i: &i32| i % 7 == 6)
        .take(3)
        .map(|i| format!("{i}|6"))
        .collect();
    assert_eq!(
        vals(&mut e, "SELECT id, g FROM t571 ORDER BY g DESC, id DESC LIMIT 3"),
        want
    );
}

/// A projection whose width VARIES between rows would be the way a
/// recycled buffer leaks — `CASE` gives every row the same arity, so
/// this checks the values instead: each row's own, none of its
/// predecessor's.
#[test]
fn round571_recycled_buffers_carry_no_stale_values() {
    let mut e = engine();
    let got = vals(
        &mut e,
        "SELECT id, CASE WHEN g = 0 THEN NULL ELSE t END FROM t571 ORDER BY id DESC LIMIT 8",
    );
    assert_eq!(got.len(), 8);
    for (offset, line) in got.iter().enumerate() {
        let id = 5000 - offset as i32;
        let (l, r) = line.split_once('|').expect("two columns");
        assert_eq!(l, id.to_string(), "{line}");
        if id % 7 == 0 {
            assert_eq!(r, "NULL", "row {id} projects NULL");
        } else {
            assert_eq!(r, format!("row{id}"), "row {id}");
        }
    }
}

/// NULLs in the sort key, which take a different branch of the key
/// builder and so a different shape of recycled buffer.
#[test]
fn round571_null_sort_keys_survive_recycling() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n571 (id INT, v INT)").unwrap();
    e.execute("INSERT INTO n571 SELECT gg, CASE WHEN gg % 3 = 0 THEN NULL ELSE gg END FROM generate_series(1, 3000) gg")
        .unwrap();
    // PG puts NULLs last in ASC, first in DESC.
    let asc = vals(&mut e, "SELECT v FROM n571 ORDER BY v LIMIT 4");
    assert_eq!(asc, vec!["1", "2", "4", "5"]);
    let desc = vals(&mut e, "SELECT v FROM n571 ORDER BY v DESC LIMIT 4");
    assert_eq!(desc, vec!["NULL", "NULL", "NULL", "NULL"]);
    let nulls_last = vals(&mut e, "SELECT v FROM n571 ORDER BY v DESC NULLS LAST LIMIT 3");
    assert_eq!(nulls_last, vec!["2999", "2998", "2996"]);
}

/// OFFSET counts toward what the accumulator must keep, and the shapes
/// that never stream keep their own path.
#[test]
fn round571_offset_and_the_shapes_that_do_not_stream() {
    let mut e = engine();
    assert_eq!(
        vals(&mut e, "SELECT id FROM t571 ORDER BY id DESC LIMIT 3 OFFSET 5"),
        vec!["4995", "4994", "4993"]
    );
    // DISTINCT builds its keys after the dup probe and never pools.
    assert_eq!(
        vals(&mut e, "SELECT DISTINCT g FROM t571 ORDER BY g DESC LIMIT 3"),
        vec!["6", "5", "4"]
    );
    // WITH TIES is excluded from streaming, so the whole set is ordered.
    assert_eq!(
        vals(&mut e, "SELECT g FROM t571 ORDER BY g DESC FETCH FIRST 1 ROW WITH TIES").len(),
        714,
        "5000 rows, g = gg % 7, so 714 of them are 6"
    );
    // No LIMIT at all: the accumulator is never trimmed.
    assert_eq!(
        vals(&mut e, "SELECT id FROM t571 ORDER BY id DESC").len(),
        5000
    );
}
