//! v7.17.0 Phase 3.2 — `SELECT *, count(*)` style queries with
//! aggregates. Probe what currently works to scope the gap.

use spg_engine::Engine;

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'a')")
        .unwrap();
}

#[test]
fn bare_count_star() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = e.execute("SELECT count(*) FROM t").unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
}

#[test]
fn count_star_with_group_by() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = e
        .execute("SELECT name, count(*) FROM t GROUP BY name")
        .unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn star_and_aggregate_mixed_errors_cleanly() {
    // PG: `SELECT *, count(*) FROM t` errors "column t.id must
    // appear in the GROUP BY clause". A clean error is the
    // right behavior — the query is invalid.
    let mut e = Engine::new();
    setup(&mut e);
    let r = e.execute("SELECT *, count(*) FROM t");
    assert!(r.is_err(), "ungrouped wildcard + aggregate should error");
}

#[test]
fn count_star_over_generate_series() {
    // The Phase 3.10 corpus run found this gap: aggregates over
    // a set-returning source. count(*) reaches the per-row
    // function dispatcher instead of the aggregate path,
    // returning "unknown function count_star".
    let mut e = Engine::new();
    let r = e.execute("SELECT count(*) FROM generate_series(1, 5)");
    // Pin current behavior — known v7.17 gap. If a future
    // commit routes set-returning-source projections through
    // exec_select_with_aggregates this assertion flips.
    match r {
        Ok(spg_engine::QueryResult::Rows { rows, .. }) => {
            assert_eq!(rows.len(), 1);
        }
        Err(err) => {
            // Documented gap, but the error must be the
            // descriptive one (not a panic / corruption).
            let msg = format!("{err:?}");
            assert!(
                msg.contains("count_star") || msg.contains("aggregate"),
                "unexpected error shape: {msg}"
            );
        }
        _ => panic!("unexpected result shape"),
    }
}

#[test]
fn star_and_aggregate_with_group_by() {
    // PG accepts this when every wildcard column appears in
    // GROUP BY (so the wildcard expansion is unambiguous).
    let mut e = Engine::new();
    setup(&mut e);
    // v7.39 (round 763, F31-C1) — the "known gap" closed: the
    // wildcard expands to the grouped columns and the shape answers.
    let r = e
        .execute("SELECT *, count(*) FROM t GROUP BY id, name")
        .unwrap();
    let spg_engine::QueryResult::Rows { rows, .. } = r else {
        panic!("expected rows");
    };
    assert_eq!(rows.len(), 3, "3 distinct (id,name) tuples");
}
