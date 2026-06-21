//! v7.17.0 Phase 3.4 — trailing `FOR { UPDATE | NO KEY UPDATE |
//! SHARE | KEY SHARE } [ OF tbl ] [ NOWAIT | SKIP LOCKED ]` lock
//! clauses on SELECT now accept and pass through. Pre-3.4 the
//! parser hard-errored on FOR, breaking every mailrs / Rails /
//! Django code path that emits `SELECT ... FOR UPDATE` for
//! advisory pessimistic locking. SPG is single-writer so the
//! clauses have no behavioural effect — the rows return the
//! same as without the clause.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn rows(r: QueryResult) -> Vec<Vec<Value<'static>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows.into_iter().map(|r| r.values).collect(),
        _ => panic!("expected rows"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t (id INT NOT NULL, name TEXT NOT NULL)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
}

#[test]
fn for_update_basic() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute("SELECT id, name FROM t WHERE id = 2 FOR UPDATE")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[0][1], Value::text("b"));
}

#[test]
fn for_share_basic() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute("SELECT id FROM t WHERE id = 1 FOR SHARE")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn for_no_key_update_pg_form() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(e.execute("SELECT id FROM t FOR NO KEY UPDATE").unwrap());
    assert_eq!(r.len(), 3);
}

#[test]
fn for_key_share_pg_form() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(e.execute("SELECT id FROM t FOR KEY SHARE").unwrap());
    assert_eq!(r.len(), 3);
}

#[test]
fn for_update_nowait() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute("SELECT id FROM t WHERE id = 3 FOR UPDATE NOWAIT")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
}

#[test]
fn for_update_skip_locked() {
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute("SELECT id FROM t FOR UPDATE SKIP LOCKED")
            .unwrap(),
    );
    assert_eq!(r.len(), 3);
}

#[test]
fn for_update_of_specific_table() {
    let mut e = Engine::new();
    setup(&mut e);
    // mailrs emits `FOR UPDATE OF t1` when joining and locking
    // only one side.
    let r = rows(e.execute("SELECT id FROM t FOR UPDATE OF t").unwrap());
    assert_eq!(r.len(), 3);
}

#[test]
fn for_update_of_multiple_tables() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (1)").unwrap();
    let r = rows(
        e.execute("SELECT t.id FROM t JOIN u ON t.id = u.id FOR UPDATE OF t, u")
            .unwrap(),
    );
    assert_eq!(r.len(), 1);
}

#[test]
fn stacked_for_clauses() {
    // PG allows multiple FOR clauses chained on one SELECT.
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE TABLE u (id INT NOT NULL)").unwrap();
    e.execute("INSERT INTO u VALUES (1), (2)").unwrap();
    let r = rows(
        e.execute(
            "SELECT t.id FROM t JOIN u ON t.id = u.id \
             FOR UPDATE OF t FOR SHARE OF u",
        )
        .unwrap(),
    );
    assert!(!r.is_empty());
}

#[test]
fn for_update_after_limit_offset() {
    let mut e = Engine::new();
    setup(&mut e);
    // mailrs's pagination + lock-for-update pattern.
    let r = rows(
        e.execute("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1 FOR UPDATE")
            .unwrap(),
    );
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[1][0], Value::Int(3));
}

#[test]
fn for_update_skip_locked_with_of_and_limit() {
    // The classic "claim next job" pattern from queue-style
    // workloads. mailrs's pending-message dequeue uses this.
    let mut e = Engine::new();
    setup(&mut e);
    let r = rows(
        e.execute(
            "SELECT id FROM t ORDER BY id LIMIT 1 \
             FOR UPDATE OF t SKIP LOCKED",
        )
        .unwrap(),
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

#[test]
fn returns_same_rows_with_and_without_lock_clause() {
    let mut e = Engine::new();
    setup(&mut e);
    let plain = rows(e.execute("SELECT id, name FROM t ORDER BY id").unwrap());
    let locked = rows(
        e.execute("SELECT id, name FROM t ORDER BY id FOR UPDATE")
            .unwrap(),
    );
    assert_eq!(plain, locked, "FOR UPDATE is a parser-noop in v7.17");
}
