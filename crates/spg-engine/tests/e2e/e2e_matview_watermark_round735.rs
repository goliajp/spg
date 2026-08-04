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
