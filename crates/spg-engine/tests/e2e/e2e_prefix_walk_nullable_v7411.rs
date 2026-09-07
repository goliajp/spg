//! v7.40.11 — a composite index could not order a NULLABLE column, so
//! the reporter's production shape sorted the table.
//!
//! sentori §3.12. Their shape is
//! `WHERE project_id = ? ORDER BY received_at DESC LIMIT 20` over an
//! index on `(project_id, received_at)` — the equality picks a prefix
//! group and the index already holds that group in `received_at` order,
//! so PostgreSQL reads twenty entries and stops.
//!
//! SPG has walked a prefix like that since v7.39.13, and refused it
//! whenever the ordering column was NULLABLE:
//!
//! ```text
//!   received_at TIMESTAMPTZ            Sort  ->  Index Scan (Index Cond)
//!   received_at TIMESTAMPTZ NOT NULL   Index Scan (Order By), no Sort
//! ```
//!
//! `NOT NULL` is not the default, so the common declaration paid for
//! the uncommon one — the same sentence r1046 wrote when it lifted this
//! exact refusal from the LEADING-column walk.
//!
//! And the machinery it needed was already there. Both walks feed ONE
//! stream, which emits the NULL-keyed rows in a separate pass at the
//! end SQL puts them (first for `NULLS FIRST`, last otherwise) — r1046
//! built it, the prefix gate never asked for it. Capability present,
//! routing absent.
//!
//! Every expectation below is PostgreSQL's placement rule: `ASC`
//! defaults to `NULLS LAST`, `DESC` to `NULLS FIRST`.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ints(e: &mut Engine, sql: &str) -> Vec<Option<i32>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r.values[0] {
                Value::Int(n) => Some(n),
                Value::Null => None,
                ref o => panic!("{sql}: {o:?}"),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn plan(e: &mut Engine, sql: &str) -> String {
    match e.execute(&format!("EXPLAIN {sql}")).expect("explain") {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("{other:?}"),
    }
}

/// Ten rows in one group, three of them with a NULL ordering key, so
/// every placement rule below has something to place.
fn fixture() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE ev (id INT PRIMARY KEY, project_id INT NOT NULL, seq INT)",
        "CREATE INDEX ev_pr ON ev (project_id, seq)",
        // group 3: seq 1,2,3 and three NULLs; group 4 is the neighbour
        // that must never leak in.
        "INSERT INTO ev VALUES (1,3,2),(2,3,1),(3,3,3),(4,3,NULL),(5,3,NULL),(6,3,NULL)",
        "INSERT INTO ev VALUES (7,4,9),(8,4,NULL)",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("setup {sql:?}: {err}"));
    }
    e
}

/// The rows, with PostgreSQL's default NULL placement in each
/// direction. This is the half that must be right whatever the plan is.
#[test]
fn the_prefix_walk_places_nulls_where_sql_puts_them() {
    let mut e = fixture();
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq"
        ),
        vec![Some(1), Some(2), Some(3), None, None, None],
        "ASC defaults to NULLS LAST"
    );
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq DESC"
        ),
        vec![None, None, None, Some(3), Some(2), Some(1)],
        "DESC defaults to NULLS FIRST"
    );
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq NULLS FIRST"
        ),
        vec![None, None, None, Some(1), Some(2), Some(3)]
    );
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq DESC NULLS LAST"
        ),
        vec![Some(3), Some(2), Some(1), None, None, None]
    );
}

/// The neighbouring group must never leak in — the NULL pass looks for
/// NULL-keyed rows, and `project_id = 4` has one.
#[test]
fn the_neighbouring_groups_rows_stay_out() {
    let mut e = fixture();
    for order in ["seq", "seq DESC", "seq NULLS FIRST", "seq DESC NULLS LAST"] {
        let got = ints(
            &mut e,
            &format!("SELECT id FROM ev WHERE project_id = 3 ORDER BY {order}"),
        );
        assert_eq!(got.len(), 6, "ORDER BY {order}: {got:?}");
        assert!(
            got.iter()
                .all(|v| matches!(v, Some(n) if (1..=6).contains(n))),
            "ORDER BY {order}: a row from project_id = 4 leaked in: {got:?}"
        );
    }
}

/// The LIMIT, which is the reporter's shape and the reason the walk
/// exists: twenty entries off the end of an index instead of a sorted
/// table.
#[test]
fn a_limit_takes_the_first_rows_in_order() {
    let mut e = fixture();
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq LIMIT 2"
        ),
        vec![Some(1), Some(2)]
    );
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq DESC NULLS LAST LIMIT 2"
        ),
        vec![Some(3), Some(2)]
    );
    // NULLS FIRST with a LIMIT takes the NULLs, which is what SQL asks
    // for and the shape whose pass cannot be skipped.
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev WHERE project_id = 3 ORDER BY seq DESC LIMIT 2"
        ),
        vec![None, None]
    );
}

/// And the plan no longer sorts. This gate produces the plan line
/// itself, so the line is a faithful witness of the decision — the row
/// assertions above are what guard the answer.
#[test]
fn the_plan_stops_sorting_a_nullable_ordering_column() {
    let mut e = fixture();
    let p = plan(
        &mut e,
        "SELECT id FROM ev WHERE project_id = 3 ORDER BY seq DESC LIMIT 20",
    );
    assert!(
        p.contains("Order By:"),
        "the index serves the ordering:\n{p}"
    );
    assert!(!p.contains("Sort  ("), "and nothing sorts:\n{p}");
}

/// The control: a NOT NULL ordering column took this road already and
/// must keep taking it, unchanged.
#[test]
fn a_not_null_ordering_column_is_unchanged() {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE ev2 (id INT PRIMARY KEY, project_id INT NOT NULL, seq INT NOT NULL)",
        "CREATE INDEX ev2_pr ON ev2 (project_id, seq)",
        "INSERT INTO ev2 VALUES (1,3,2),(2,3,1),(3,3,3)",
    ] {
        e.execute(sql).expect("setup");
    }
    assert_eq!(
        ints(
            &mut e,
            "SELECT seq FROM ev2 WHERE project_id = 3 ORDER BY seq DESC"
        ),
        vec![Some(3), Some(2), Some(1)]
    );
    let p = plan(
        &mut e,
        "SELECT id FROM ev2 WHERE project_id = 3 ORDER BY seq DESC LIMIT 20",
    );
    assert!(p.contains("Order By:"), "{p}");
    assert!(!p.contains("Sort  ("), "{p}");
}
