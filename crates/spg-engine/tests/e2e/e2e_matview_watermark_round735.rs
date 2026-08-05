//! Round 735 (S14/B3 first knife) — the materialized-view refresh
//! watermark. When a view's full dependency set is provable and no
//! dependency changed since the last refresh, REFRESH is an O(1) no-op
//! with an IDENTICAL observable result — PG recomputes unconditionally.
//!
//! Correctness is the whole game here: a wrong no-op serves stale data
//! silently. Every test drives the view through change-then-refresh
//! cycles and asserts the CONTENT, not the plumbing.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => spg_engine::eval::value_to_text(&rows[0].values[0]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round735_refresh_sees_every_kind_of_base_change() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b735 (id INT, g INT)").unwrap();
    e.execute("INSERT INTO b735 SELECT gg, gg % 3 FROM generate_series(1, 30) gg")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv735 AS SELECT g, count(*) c FROM b735 GROUP BY g")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT sum(c) FROM mv735"), "30");
    // No change -> refresh (the no-op path) keeps the answer.
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(one(&mut e, "SELECT sum(c) FROM mv735"), "30");
    // INSERT is seen.
    e.execute("INSERT INTO b735 VALUES (31, 1)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(one(&mut e, "SELECT sum(c) FROM mv735"), "31");
    // UPDATE is seen.
    e.execute("UPDATE b735 SET g = 0 WHERE id = 31").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(
        one(&mut e, "SELECT c FROM mv735 WHERE g = 0"),
        "11",
        "the moved row lands in group 0"
    );
    // DELETE is seen.
    e.execute("DELETE FROM b735 WHERE id > 20").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(one(&mut e, "SELECT sum(c) FROM mv735"), "20");
    // TRUNCATE is seen.
    e.execute("TRUNCATE b735").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv735"), "0");
}

#[test]
fn round735_with_no_data_never_no_ops() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b735 (id INT)").unwrap();
    e.execute("INSERT INTO b735 VALUES (1), (2)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv735 AS SELECT id FROM b735").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv735"), "2");
    // WITH NO DATA empties even though nothing changed since the build.
    e.execute("REFRESH MATERIALIZED VIEW mv735 WITH NO DATA").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv735"), "0");
    // And the next plain REFRESH repopulates (the NO DATA cleared the
    // watermark; a no-op here would leave the view empty forever).
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv735"), "2");
}

#[test]
fn round735_a_second_dependency_counts() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE l735 (id INT, v INT)").unwrap();
    e.execute("CREATE TABLE r735 (id INT, w INT)").unwrap();
    e.execute("INSERT INTO l735 VALUES (1, 10), (2, 20)").unwrap();
    e.execute("INSERT INTO r735 VALUES (1, 100), (2, 200)").unwrap();
    e.execute(
        "CREATE MATERIALIZED VIEW mv735 AS \
         SELECT l.id, l.v + r.w s FROM l735 l JOIN r735 r ON l.id = r.id",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT sum(s) FROM mv735"), "330");
    // A write to the SECOND table alone must invalidate.
    e.execute("UPDATE r735 SET w = 300 WHERE id = 2").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv735").unwrap();
    assert_eq!(one(&mut e, "SELECT sum(s) FROM mv735"), "430");
}

/// v7.39 (round 737, S14/B3 knife 2) — INSERT-ONLY delta application.
/// A maintainable view (single stored table, pure projection, pure
/// WHERE) whose buffered changes are all Inserts refreshes by running
/// JUST those rows through the projection and appending. Content is
/// the only witness that matters: every cycle asserts what a full
/// recompute would produce.
#[test]
fn round737_insert_only_delta_matches_full_recompute() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b737 (id INT, g INT)").unwrap();
    e.execute("INSERT INTO b737 SELECT gg, gg % 5 FROM generate_series(1, 50) gg")
        .unwrap();
    e.execute(
        "CREATE MATERIALIZED VIEW mv737 AS SELECT id * 10 tenfold, g FROM b737 WHERE g <> 3",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv737"), "40");
    // Insert-only cycle: two kept rows, one filtered by the WHERE.
    e.execute("INSERT INTO b737 VALUES (51, 1), (52, 3), (53, 4)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv737").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv737"), "42");
    assert_eq!(one(&mut e, "SELECT sum(tenfold) FROM mv737 WHERE tenfold > 500"), "1040");
    // Another insert-only cycle stacks on the first.
    e.execute("INSERT INTO b737 VALUES (54, 0)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv737").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv737"), "43");
    // A DELETE poisons the buffer -> full path, content still exact.
    e.execute("DELETE FROM b737 WHERE id <= 10").unwrap();
    e.execute("INSERT INTO b737 VALUES (55, 1)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv737").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv737"), "36");
    // And the cycle AFTER a full refresh is insert-only again.
    e.execute("INSERT INTO b737 VALUES (56, 2)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv737").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv737"), "37");
    // Full-recompute cross-check: rebuild from scratch and compare.
    e.execute("DROP MATERIALIZED VIEW mv737").unwrap();
    e.execute(
        "CREATE MATERIALIZED VIEW mv737 AS SELECT id * 10 tenfold, g FROM b737 WHERE g <> 3",
    )
    .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv737"), "37");
}

/// An UPDATE in the buffer must fall back to the full path (its delta
/// machinery is the next knife) — content stays exact either way.
#[test]
fn round737_update_falls_back_to_full() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b737 (id INT, v INT)").unwrap();
    e.execute("INSERT INTO b737 VALUES (1, 10), (2, 20)").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv737 AS SELECT v FROM b737").unwrap();
    e.execute("UPDATE b737 SET v = 99 WHERE id = 2").unwrap();
    e.execute("INSERT INTO b737 VALUES (3, 30)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv737").unwrap();
    assert_eq!(one(&mut e, "SELECT sum(v) FROM mv737"), "139");
}

/// v7.39 (round 738, S14/B3 knife 3) — DELETE and tombstone deltas
/// apply through the row map (built by the maintainable full refresh's
/// internal scan). Content is the witness: mixed insert/delete cycles,
/// a WHERE-filtered base row whose delete touches nothing, and a
/// from-scratch rebuild cross-check.
#[test]
fn round738_delete_delta_matches_full_recompute() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b738 (id INT, g INT)").unwrap();
    e.execute("INSERT INTO b738 SELECT gg, gg % 4 FROM generate_series(1, 40) gg")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv738 AS SELECT id, g FROM b738 WHERE g <> 2")
        .unwrap();
    // A full refresh installs the row map (CREATE ran the SQL path).
    e.execute("INSERT INTO b738 VALUES (41, 0)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv738").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv738"), "31");
    // Delete-only cycle: id 1..=8 removes 6 view rows (two are g=2,
    // never in the view — their deletes must touch nothing).
    e.execute("DELETE FROM b738 WHERE id <= 8").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv738").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv738"), "25");
    assert_eq!(one(&mut e, "SELECT min(id) FROM mv738"), "9");
    // Mixed cycle IN ORDER: insert then delete THAT row, plus one
    // surviving insert.
    e.execute("INSERT INTO b738 VALUES (100, 1)").unwrap();
    e.execute("DELETE FROM b738 WHERE id = 100").unwrap();
    e.execute("INSERT INTO b738 VALUES (101, 3)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv738").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv738"), "26");
    assert_eq!(one(&mut e, "SELECT max(id) FROM mv738"), "101");
    // Cross-check against a from-scratch rebuild.
    e.execute("DROP MATERIALIZED VIEW mv738").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv738 AS SELECT id, g FROM b738 WHERE g <> 2")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv738"), "26");
}

/// v7.39 (round 739, S14/B3 knife 4) — the UPDATE delta arm: four
/// quadrants of (old row in view?) x (new row passes WHERE?). Content
/// witnesses each: in-place value change, a row LEAVING the view, a
/// row ENTERING it, and an update entirely outside it — followed by
/// deletes over the updated map and a from-scratch cross-check.
#[test]
fn round739_update_delta_matches_full_recompute() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE b739 (id INT, g INT, v INT)").unwrap();
    e.execute("INSERT INTO b739 SELECT gg, gg % 3, gg * 100 FROM generate_series(1, 30) gg")
        .unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv739 AS SELECT id, v FROM b739 WHERE g <> 1")
        .unwrap();
    // Install the row map via a full (internal-scan) refresh.
    e.execute("INSERT INTO b739 VALUES (31, 0, 3100)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv739").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv739"), "21");
    // (in view, stays): value changes in place.
    e.execute("UPDATE b739 SET v = 999 WHERE id = 3").unwrap();
    // (in view, leaves): g flips to the filtered value.
    e.execute("UPDATE b739 SET g = 1 WHERE id = 6").unwrap();
    // (outside, enters): a g=1 row flips in.
    e.execute("UPDATE b739 SET g = 2 WHERE id = 4").unwrap();
    // (outside, stays outside): invisible either way.
    e.execute("UPDATE b739 SET v = 1 WHERE id = 7").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv739").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv739"), "21");
    assert_eq!(one(&mut e, "SELECT v FROM mv739 WHERE id = 3"), "999");
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv739 WHERE id = 6"), "0");
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv739 WHERE id = 4"), "1");
    // Deletes over the updated map still resolve correctly.
    e.execute("DELETE FROM b739 WHERE id IN (3, 4, 5)").unwrap();
    e.execute("REFRESH MATERIALIZED VIEW mv739").unwrap();
    // 21 - 3: id 3 (in), id 4 (entered above), id 5 (g=2, in) all leave.
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv739"), "18");
    // Cross-check.
    e.execute("DROP MATERIALIZED VIEW mv739").unwrap();
    e.execute("CREATE MATERIALIZED VIEW mv739 AS SELECT id, v FROM b739 WHERE g <> 1")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM mv739"), "18");
}
