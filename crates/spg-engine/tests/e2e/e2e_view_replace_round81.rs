//! v7.39 (read01 round 81) — a sweep of the view / CTE / recursive-CTE surface.
//! Recursive hierarchy walks, fib via a 3-column recursive CTE, UNION vs UNION
//! ALL dedup in the recursive term, `WITH ... (VALUES ...)`, chained CTEs,
//! view-over-view, a data-modifying CTE (`DELETE ... RETURNING`): all already
//! matched PG. Two things did not, and both were SPG being too PERMISSIVE.
//!
//! 1. CREATE OR REPLACE VIEW may only APPEND columns. PG forbids renaming,
//!    dropping, reordering or retyping any existing column. SPG accepted every
//!    one of these and silently swapped the view's shape — after which a
//!    downstream `SELECT known_col FROM v` resolves to a different column or
//!    vanishes. That is data corruption wearing a successful DDL.
//!
//! 2. A missing column reported "column not found: x". PG says
//!    `column "x" does not exist` — and that phrasing is what the wire layer's
//!    SQLSTATE table keys 42703 off, so before this the error also reached the
//!    client under the generic error class.

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE emp (id int, name text, sal int)")
        .unwrap();
    e.execute("INSERT INTO emp VALUES (1,'a',100),(2,'b',90),(3,'c',80)")
        .unwrap();
    e.execute("CREATE VIEW v AS SELECT id, name, sal FROM emp")
        .unwrap();
}

fn err(e: &mut Engine, sql: &str) -> String {
    // Display (the user-facing / wire wording), not Debug (the struct dump).
    e.execute(sql).unwrap_err().to_string()
}

fn joined(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(","),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn a_replace_view_may_only_append_columns() {
    let mut e = Engine::new();
    setup(&mut e);
    // Same names, retype the body expression: allowed (sal stays integer here).
    e.execute("CREATE OR REPLACE VIEW v AS SELECT id, name, sal*2 sal FROM emp")
        .unwrap();
    assert_eq!(
        joined(&mut e, "SELECT sal FROM v ORDER BY sal"),
        "160,180,200"
    );
    // Append a column at the end: allowed.
    e.execute("CREATE OR REPLACE VIEW v AS SELECT id, name, sal*2 sal, 1 extra FROM emp")
        .unwrap();

    // Rename an existing column: rejected. (Kept at the same column count as the
    // current 4-column view; renaming while ALSO dropping would trip the drop
    // check first, which is what PG reports too.)
    assert!(
        err(
            &mut e,
            "CREATE OR REPLACE VIEW v AS SELECT id, name AS nom, sal*2 sal, 1 extra FROM emp"
        )
        .contains("cannot change name of view column"),
    );
    // Drop a column: rejected.
    assert!(
        err(
            &mut e,
            "CREATE OR REPLACE VIEW v AS SELECT id, name FROM emp"
        )
        .contains("cannot drop columns from view"),
    );
    // Change a column's type: rejected.
    assert!(
        err(
            &mut e,
            "CREATE OR REPLACE VIEW v AS SELECT id, name, sal::text sal, 1 extra FROM emp"
        )
        .contains("cannot change data type of view column"),
    );
    // Reorder (which reads as a rename at the first differing position): rejected.
    assert!(
        err(
            &mut e,
            "CREATE OR REPLACE VIEW v AS SELECT name, id, sal*2 sal, 1 extra FROM emp"
        )
        .contains("cannot change name of view column"),
    );
    // After all the rejected attempts the view is still its last GOOD shape.
    assert_eq!(joined(&mut e, "SELECT extra FROM v LIMIT 1"), "1");
}

#[test]
fn b_missing_column_uses_pg_wording() {
    let mut e = Engine::new();
    setup(&mut e);
    assert!(err(&mut e, "SELECT nope FROM emp").contains("column \"nope\" does not exist"));
}

#[test]
fn c_recursive_and_chained_ctes_still_hold() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE emp (id int, mgr int, name text)")
        .unwrap();
    e.execute("INSERT INTO emp VALUES (1,NULL,'a'),(2,1,'b'),(3,1,'c'),(4,2,'d')")
        .unwrap();
    // Depth-tagged hierarchy walk.
    assert_eq!(
        joined(
            &mut e,
            "SELECT id||':'||lvl FROM (WITH RECURSIVE r(id,lvl) AS \
             (SELECT id,0 FROM emp WHERE mgr IS NULL \
              UNION ALL SELECT e.id, r.lvl+1 FROM emp e JOIN r ON e.mgr=r.id) \
             SELECT id, lvl FROM r ORDER BY id) s"
        ),
        "1:0,2:1,3:1,4:2"
    );
    // fib(1..8) via a 3-column recursive CTE.
    assert_eq!(
        joined(
            &mut e,
            "SELECT a::text FROM (WITH RECURSIVE f(n,a,b) AS \
             (SELECT 1,0,1 UNION ALL SELECT n+1,b,a+b FROM f WHERE n<8) \
             SELECT a FROM f) s"
        ),
        "0,1,1,2,3,5,8,13"
    );
    // UNION (not ALL) dedups the working set.
    assert_eq!(
        joined(
            &mut e,
            "SELECT count(*)::text FROM (WITH RECURSIVE r(n) AS \
             (SELECT 1 UNION SELECT n+1 FROM r WHERE n<3) SELECT n FROM r) s"
        ),
        "3"
    );
}

#[test]
fn d_data_modifying_cte_nested_is_rejected_with_pg_wording() {
    let mut e = Engine::new();
    setup(&mut e);
    // A data-modifying CTE nested in a subquery: PG requires it at top level.
    let msg = err(
        &mut e,
        "SELECT x FROM (WITH d AS (DELETE FROM emp WHERE id=3 RETURNING id) SELECT id x FROM d) s",
    );
    assert!(msg.contains("must be at the top level"), "got {msg}");
}
