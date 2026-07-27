//! v7.39 (round 569) — min and max join the fused aggregate lane.
//!
//! Round 568 measured min/max costing DOUBLE a sum over the same scan
//! and could not name why; the guess it did make was refuted. The answer
//! was in `fused_layout`: it accepted `count(*)`, `count`, `sum` and
//! `avg` and rejected everything else, so min and max fell through to
//! the generic per-spec machinery — and missed the shard-parallel scan
//! the fused path runs. That is the whole of the 2x.
//!
//! Over pgwire, 500k INT rows, three paired batches, medians:
//!
//!                    before    after     PG18    ratio
//!     max(id)         29.5 ms   17.8 ms   8.5    3.47x -> 2.09x
//!     min(id)         23.5      15.9      8.1    2.90x -> 1.96x
//!     max(id),min(id) 34.7      21.4      9.3    3.73x -> 2.30x
//!     GROUP BY, max   37.7      22.5     20.6    1.83x -> 1.09x
//!
//! Ranges disjoint. The GROUP BY row is not a separate change: the
//! parallel GROUP BY path shares the same fused ops, so it picked the
//! extremes up on its own.
//!
//! What the lane must NOT take, and why each is here:
//!
//!   * an ENUM argument — the index orders labels lexicographically
//!     while PG orders them by catalog position, and the fused lane
//!     carries no labels. Those keep the generic path.
//!   * FILTER / DISTINCT / a second argument / ORDER BY — the layout
//!     already refused these for sum and count; extremes inherit it.
//!
//! NULL contributes nothing and a group of only NULLs answers NULL,
//! which is PG's rule and was the generic path's.

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
    e.execute("CREATE TABLE f569 (id INT, g INT, t TEXT, n NUMERIC, d DATE)")
        .unwrap();
    e.execute(
        "INSERT INTO f569 SELECT gg, gg % 10, 'r' || gg, gg * 1.5, DATE '2026-01-01' + gg \
         FROM generate_series(1, 400) gg",
    )
    .unwrap();
    e
}

/// The extremes, over every type the lane can carry.
#[test]
fn round569_extremes_answer() {
    let mut e = engine();
    assert_eq!(vals(&mut e, "SELECT min(id), max(id) FROM f569"), vec!["1|400"]);
    assert_eq!(vals(&mut e, "SELECT min(g), max(g) FROM f569"), vec!["0|9"]);
    assert_eq!(
        vals(&mut e, "SELECT min(t), max(t) FROM f569"),
        vec!["r1|r99"],
        "text orders lexicographically"
    );
    assert_eq!(
        vals(&mut e, "SELECT min(n), max(n) FROM f569"),
        vec!["1.5|600.0"]
    );
    assert_eq!(
        vals(&mut e, "SELECT min(d), max(d) FROM f569"),
        vec!["2026-01-02|2027-02-05"]
    );
    // Alongside the aggregates that were already fused.
    assert_eq!(
        vals(&mut e, "SELECT count(*), sum(id), min(id), max(id) FROM f569"),
        vec!["400|80200|1|400"]
    );
    // Two extremes over the SAME column must not share one accumulator.
    assert_eq!(
        vals(&mut e, "SELECT max(id), max(id), min(id) FROM f569"),
        vec!["400|400|1"]
    );
}

/// NULL contributes nothing; a group of only NULLs answers NULL.
#[test]
fn round569_nulls_follow_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE n569 (id INT, v INT)").unwrap();
    e.execute("INSERT INTO n569 VALUES (1, NULL), (2, 5), (3, NULL), (4, 2)")
        .unwrap();
    assert_eq!(vals(&mut e, "SELECT min(v), max(v) FROM n569"), vec!["2|5"]);
    assert_eq!(
        vals(&mut e, "SELECT count(v), min(v), max(v) FROM n569"),
        vec!["2|2|5"]
    );
    e.execute("CREATE TABLE a569 (v INT)").unwrap();
    e.execute("INSERT INTO a569 VALUES (NULL), (NULL)").unwrap();
    assert_eq!(vals(&mut e, "SELECT min(v), max(v) FROM a569"), vec!["NULL|NULL"]);
    // An empty table answers NULL too.
    e.execute("CREATE TABLE e569 (v INT)").unwrap();
    assert_eq!(vals(&mut e, "SELECT min(v), max(v) FROM e569"), vec!["NULL|NULL"]);
}

/// GROUP BY shares the same fused ops, so the answers must match the
/// ungrouped ones group by group.
#[test]
fn round569_group_by_extremes() {
    let mut e = engine();
    let got = vals(
        &mut e,
        "SELECT g, min(id), max(id), count(*) FROM f569 GROUP BY g ORDER BY g",
    );
    assert_eq!(got.len(), 10);
    assert_eq!(got[0], "0|10|400|40");
    assert_eq!(got[1], "1|1|391|40");
    assert_eq!(got[9], "9|9|399|40");
    // A NULL group key keeps its own extremes.
    e.execute("INSERT INTO f569 VALUES (500, NULL, 'z', 1.0, DATE '2026-01-01')")
        .unwrap();
    let with_null = vals(
        &mut e,
        "SELECT g, min(id), max(id) FROM f569 WHERE g IS NULL GROUP BY g",
    );
    assert_eq!(with_null, vec!["NULL|500|500"]);
}

/// An enum argument keeps the generic path, because the fused lane
/// carries no labels and an enum orders by catalog position.
#[test]
fn round569_enum_extremes_use_member_order() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mood569 AS ENUM ('sad', 'ok', 'happy')")
        .unwrap();
    e.execute("CREATE TABLE m569 (id INT, m mood569)").unwrap();
    e.execute("INSERT INTO m569 VALUES (1,'happy'),(2,'sad'),(3,'ok')")
        .unwrap();
    // Lexicographically 'happy' < 'ok' < 'sad'; by member order
    // 'sad' < 'ok' < 'happy'. PG answers by member order.
    assert_eq!(
        vals(&mut e, "SELECT min(m), max(m) FROM m569"),
        vec!["sad|happy"]
    );
    assert_eq!(
        vals(&mut e, "SELECT m, min(m) FROM m569 GROUP BY m ORDER BY m"),
        vec!["sad|sad", "ok|ok", "happy|happy"]
    );
}

/// The clauses the layout has always refused still take the generic
/// path, and still answer.
#[test]
fn round569_refused_clauses_still_answer() {
    let mut e = engine();
    assert_eq!(
        vals(&mut e, "SELECT max(id) FILTER (WHERE g = 1) FROM f569"),
        vec!["391"]
    );
    assert_eq!(
        vals(&mut e, "SELECT min(DISTINCT g), max(DISTINCT g) FROM f569"),
        vec!["0|9"]
    );
    // An expression argument is not a bound column.
    assert_eq!(vals(&mut e, "SELECT max(id + 1) FROM f569"), vec!["401"]);
    // And the extremes still agree with an ORDER BY / LIMIT reading.
    assert_eq!(
        vals(&mut e, "SELECT id FROM f569 ORDER BY id DESC LIMIT 1"),
        vec!["400"]
    );
}
