//! v7.39.13 — `WHERE lead = ? ORDER BY next DESC LIMIT n` walks the
//! index instead of sorting the table.
//!
//! Sentori's busiest read, reported unchanged for three versions:
//! `WHERE project_id = ? ORDER BY received_at DESC LIMIT 20` behind an
//! index on `(project_id, received_at)`. PostgreSQL 18.6 plans
//!
//! ```text
//!   Limit
//!     -> Index Scan Backward using ev_proj_recv on ev
//!          Index Cond: (project_id = 7)
//! ```
//!
//! and SPG planned `Sort -> Seq Scan`: the whole table sorted to return
//! twenty rows. The ordered walk that existed could only start at an
//! index's LEADING column, so an index on `(project_id, received_at)`
//! served `ORDER BY project_id` and nothing else.
//!
//! What was missing is underneath: a tree walk bounded by a key prefix.
//! `PersistentBTreeMap::range_rev_by` descends to the group's top in
//! `O(log N)` and walks left; the bound is two predicates rather than
//! two keys because a tuple `[p]` sorts BELOW every longer tuple that
//! starts with `p`, so no single key names the group's top.
//!
//! These rows pin the ANSWER. The plan is pinned beside them because
//! r1044's rule is that the executor and EXPLAIN must not disagree —
//! the walk running while the plan says `Seq Scan` is the defect that
//! rule exists for, and wiring only the executor would recreate it.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}"))
    else {
        panic!("{sql}: expected Rows")
    };
    rows.iter()
        .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
        .collect()
}

/// 8 projects x 500 rows. Interleaved on purpose: project 7's rows are
/// scattered through the heap, so a walk that fell back to a scan would
/// still answer and only the plan would tell.
fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ev (id bigint NOT NULL, project_id int NOT NULL, seq int NOT NULL)")
        .unwrap();
    let mut sql = String::from("INSERT INTO ev VALUES ");
    for g in 0..4000u32 {
        if g > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({}, {}, {})", g, (g % 8) + 1, g));
    }
    e.execute(&sql).unwrap();
    e.execute("CREATE INDEX ev_proj_seq ON ev (project_id, seq)")
        .unwrap();
    e
}

#[test]
fn the_last_rows_of_one_group_come_back_in_order() {
    let mut e = seeded();
    // project 7 holds g where g % 8 == 6: … 3998, 3990, 3982, 3974, 3966
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE project_id = 7 ORDER BY seq DESC LIMIT 5",
    );
    assert_eq!(got, ["3998", "3990", "3982", "3974", "3966"]);
}

#[test]
fn the_first_rows_of_one_group_come_back_in_order() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE project_id = 7 ORDER BY seq ASC LIMIT 5",
    );
    assert_eq!(got, ["6", "14", "22", "30", "38"]);
}

/// The walk must not leak its neighbours — the failure a descent that
/// lands one position off produces, and the one a missing lower bound
/// produces.
#[test]
fn the_walk_stays_inside_its_group() {
    let mut e = seeded();
    let got = rows(&mut e, "SELECT count(*) FROM ev WHERE project_id = 7");
    assert_eq!(got, ["500"]);
    // Every row the walk returns belongs to the group, unbounded.
    let all = rows(
        &mut e,
        "SELECT project_id FROM ev WHERE project_id = 7 ORDER BY seq DESC",
    );
    assert_eq!(all.len(), 500);
    assert!(all.iter().all(|p| p == "7"), "a neighbour leaked in");
}

/// OFFSET counts rows that pass, and the walk applies it as it goes.
#[test]
fn offset_inside_the_group() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE project_id = 7 ORDER BY seq DESC OFFSET 2 LIMIT 3",
    );
    assert_eq!(got, ["3982", "3974", "3966"]);
}

/// A residual predicate still runs: the prefix only narrows the walk.
#[test]
fn a_second_predicate_still_filters() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE project_id = 7 AND id < 3980 ORDER BY seq DESC LIMIT 3",
    );
    assert_eq!(got, ["3974", "3966", "3958"]);
}

/// The empty group answers nothing rather than sliding into a
/// neighbour.
#[test]
fn an_absent_group_is_empty() {
    let mut e = seeded();
    let got = rows(
        &mut e,
        "SELECT id FROM ev WHERE project_id = 99 ORDER BY seq DESC LIMIT 5",
    );
    assert!(got.is_empty(), "{got:?}");
}

/// r1044 — and EXPLAIN says what runs.
#[test]
fn the_plan_names_the_index_walk() {
    let mut e = seeded();
    let plan = rows(
        &mut e,
        "EXPLAIN SELECT id FROM ev WHERE project_id = 7 ORDER BY seq DESC LIMIT 5",
    )
    .join(" | ");
    assert!(
        plan.contains("ev_proj_seq"),
        "the plan must name the index the executor walks: {plan}"
    );
    assert!(
        !plan.contains("Sort"),
        "a walk that runs while the plan says Sort is the r1044 defect: {plan}"
    );
}
