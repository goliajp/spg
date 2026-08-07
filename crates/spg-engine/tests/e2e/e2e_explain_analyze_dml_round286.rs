//! v7.39 (round 286) — `EXPLAIN ANALYZE` over INSERT / UPDATE / DELETE.
//!
//! PG's ANALYZE really runs the statement. It does not plan-and-discard,
//! and it does NOT roll back — after `EXPLAIN ANALYZE INSERT …` the row
//! is there. SPG refused the whole shape with "it would execute the
//! write", which read like a policy but was a structural fact:
//! `exec_explain` takes `&self`, so the write had nowhere to happen.
//!
//! This adds the `&mut self` sibling, reached only from the write
//! dispatch. The read-only path still refuses — correctly, since a write
//! genuinely cannot run there.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn plan(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.iter()
        .map(|row| spg_engine::eval::value_to_text(&row.values[0]))
        .collect()
}

fn count(e: &mut Engine) -> String {
    let QueryResult::Rows { rows, .. } = e.execute("SELECT count(*) FROM pt9").unwrap() else {
        panic!("expected Rows");
    };
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

/// Microseconds since the epoch. `Engine::new()` carries no clock, and
/// without one ANALYZE omits both the `time=` half of the actual block
/// and the trailing `Execution Time:` line — so a pin that asserts on
/// either has to supply one, exactly as the server does.
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as i64)
}

fn fixture() -> Engine {
    let mut e = Engine::new().with_clock(now_micros);
    e.execute("CREATE TABLE pt9 (id int primary key, v text)")
        .unwrap();
    e.execute("INSERT INTO pt9 VALUES (1,'a'),(2,'b'),(3,'c')")
        .unwrap();
    e
}

#[test]
fn analyze_on_an_insert_actually_inserts() {
    // The half that makes this more than a rendering change.
    let mut e = fixture();
    assert_eq!(count(&mut e), "3");
    plan(&mut e, "EXPLAIN ANALYZE INSERT INTO pt9 VALUES (10,'y')");
    assert_eq!(count(&mut e), "4", "PG does not roll ANALYZE back");
}

#[test]
fn analyze_on_update_and_delete_take_effect_too() {
    let mut e = fixture();
    plan(&mut e, "EXPLAIN ANALYZE UPDATE pt9 SET v='q' WHERE id=1");
    let QueryResult::Rows { rows, .. } = e.execute("SELECT v FROM pt9 WHERE id=1").unwrap() else {
        panic!("expected Rows");
    };
    assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "q");

    plan(&mut e, "EXPLAIN ANALYZE DELETE FROM pt9 WHERE id=2");
    assert_eq!(count(&mut e), "2");
}

#[test]
fn the_root_node_names_the_verb_and_the_table() {
    let mut e = fixture();
    for (sql, head) in [
        (
            "EXPLAIN ANALYZE INSERT INTO pt9 VALUES (20,'x')",
            "Insert on pt9",
        ),
        (
            "EXPLAIN ANALYZE UPDATE pt9 SET v='z' WHERE id=1",
            "Update on pt9",
        ),
        (
            "EXPLAIN ANALYZE DELETE FROM pt9 WHERE id=3",
            "Delete on pt9",
        ),
    ] {
        let lines = plan(&mut e, sql);
        assert!(lines[0].starts_with(head), "{sql}: {:?}", lines[0]);
    }
}

#[test]
fn the_modify_node_estimates_zero_rows() {
    // PG's ModifyTable reports rows=0 without RETURNING — in the
    // ESTIMATE as well as the actuals. SPG carried the child's estimate
    // up, so a plain `EXPLAIN INSERT` said rows=1 where PG said rows=0.
    let mut e = fixture();
    let lines = plan(&mut e, "EXPLAIN INSERT INTO pt9 VALUES (30,'p')");
    assert!(lines[0].contains("rows=0"), "{:?}", lines[0]);
    // …and the source node keeps its own estimate.
    assert!(lines[1].contains("rows=1"), "{:?}", lines[1]);
}

#[test]
fn the_actual_block_and_execution_time_are_present() {
    let mut e = fixture();
    let lines = plan(&mut e, "EXPLAIN ANALYZE INSERT INTO pt9 VALUES (40,'m')");
    assert!(lines[0].contains("(actual time="), "{:?}", lines[0]);
    // The modify node returns nothing; the source produced one row.
    assert!(lines[0].contains("rows=0.00 loops=1"), "{:?}", lines[0]);
    assert!(lines[1].contains("rows=1.00 loops=1"), "{:?}", lines[1]);
    assert!(
        lines.last().unwrap().starts_with("Execution Time:"),
        "{:?}",
        lines.last(),
    );
}

#[test]
fn costs_off_drops_the_estimates_but_keeps_the_actuals() {
    let mut e = fixture();
    let lines = plan(
        &mut e,
        "EXPLAIN (ANALYZE, COSTS OFF) INSERT INTO pt9 VALUES (50,'n')",
    );
    assert!(!lines[0].contains("cost="), "{:?}", lines[0]);
    assert!(lines[0].contains("(actual time="), "{:?}", lines[0]);
    assert_eq!(count(&mut e), "4");
}

#[test]
fn plain_explain_of_a_write_still_does_not_execute() {
    // The distinction that matters: EXPLAIN plans, ANALYZE runs.
    let mut e = fixture();
    plan(&mut e, "EXPLAIN INSERT INTO pt9 VALUES (60,'k')");
    assert_eq!(count(&mut e), "3");
}
