//! v6.3.0 — Engine plan cache e2e.
//!
//! Verifies the engine-level cache reuses parsed plans across
//! `prepare_cached` calls and enforces the 256-entry LRU cap.

use spg_engine::Engine;

#[test]
fn repeat_prepare_returns_cached_plan() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t1 (id INT, name TEXT)")
        .expect("create");
    eng.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .expect("insert");

    assert_eq!(eng.plan_cache().len(), 0, "cache starts empty");

    let _stmt1 = eng
        .prepare_cached("SELECT * FROM t1 WHERE id = 1")
        .expect("first prepare");
    assert_eq!(eng.plan_cache().len(), 1, "first prepare populates");

    let _stmt2 = eng
        .prepare_cached("SELECT * FROM t1 WHERE id = 1")
        .expect("second prepare");
    assert_eq!(
        eng.plan_cache().len(),
        1,
        "second prepare hits cache, doesn't add a duplicate"
    );

    let _stmt3 = eng
        .prepare_cached("SELECT * FROM t1 WHERE id = 2")
        .expect("different sql");
    assert_eq!(
        eng.plan_cache().len(),
        2,
        "different SQL string adds a new entry"
    );
}

#[test]
fn lru_evicts_oldest_at_cap() {
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t1 (id INT)").expect("create");

    // Fill past cap with distinct SQL strings. Use a WHERE clause with
    // varying literal so each SQL is unique.
    for i in 0..300_u32 {
        let sql = alloc_sql(i);
        eng.prepare_cached(&sql).expect("prepare");
    }

    // Cap is 256 — verified via the engine's read-only accessor.
    assert_eq!(
        eng.plan_cache().len(),
        256,
        "cache stays at cap regardless of how many distinct SQLs we prepared"
    );
}

fn alloc_sql(i: u32) -> String {
    format!("SELECT * FROM t1 WHERE id = {i}")
}

#[test]
fn prepared_plan_runs_correctly_after_cache_hit() {
    // Smoke test: a cached plan must still execute as expected.
    let mut eng = Engine::new();
    eng.execute("CREATE TABLE t1 (id INT, name TEXT)")
        .expect("create");
    eng.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b')")
        .expect("insert");

    let r1 = eng
        .execute("SELECT name FROM t1 WHERE id = 1")
        .expect("exec1");
    // Force a cache hit by preparing the same SQL twice; second
    // execution path goes through the cached AST via the engine's
    // own execute() (since the engine internally calls prepare()).
    // We exercise the cache directly by calling prepare_cached
    // twice and then handing the second stmt back to the executor.
    let stmt_a = eng
        .prepare_cached("SELECT name FROM t1 WHERE id = 2")
        .expect("first prepare");
    let stmt_b = eng
        .prepare_cached("SELECT name FROM t1 WHERE id = 2")
        .expect("second prepare (hit)");
    assert_eq!(stmt_a, stmt_b, "cached AST must equal the first parse");

    let r2 = eng.execute_prepared(stmt_b, &[]).expect("exec via cached");
    match (r1, r2) {
        (
            spg_engine::QueryResult::Rows { rows: r1_rows, .. },
            spg_engine::QueryResult::Rows { rows: r2_rows, .. },
        ) => {
            assert_eq!(r1_rows.len(), 1, "id=1 → one row");
            assert_eq!(r2_rows.len(), 1, "id=2 → one row");
        }
        _ => panic!("expected Rows results from both selects"),
    }
}
