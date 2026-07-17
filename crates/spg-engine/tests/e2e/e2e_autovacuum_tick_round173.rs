//! v7.39 (read01 round 173) — autovacuum off the statement path.
//!
//! PG's autovacuum runs in background workers, never inside a client
//! statement. The engine now supports that split: a host with a
//! worker thread (spg-server) flips `set_autovacuum_inline(false)`
//! so the statement-exit trigger stops vacuuming inline, and drives
//! `autovacuum_tick()` on its own cadence instead. Pins:
//!   1. `autovacuum_tick` vacuums exactly the over-threshold tables
//!      (dead >= 1000 && dead*4 >= live) and reports the count.
//!   2. inline-off: the DML statement that crosses the threshold no
//!      longer vacuums; a later tick reclaims.
//!   3. tick is a no-op inside an explicit transaction (uncommitted
//!      tombstones must survive for rollback).

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(n) => n,
            spg_storage::Value::Int(n) => i64::from(n),
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn dead(e: &mut Engine, t: &str) -> i64 {
    one(
        e,
        &format!("SELECT n_dead_tup FROM pg_stat_user_tables WHERE relname = '{t}'"),
    )
}

/// 1500 rows, then delete 1200 → dead=1200 (>=1000), live=300,
/// dead*4 >= live — over threshold. A second table stays under.
fn setup_over_and_under(e: &mut Engine) {
    e.execute("CREATE TABLE big(id INT PRIMARY KEY)").unwrap();
    e.execute("CREATE TABLE small(id INT PRIMARY KEY)").unwrap();
    e.execute("INSERT INTO big SELECT g FROM generate_series(1, 1500) g")
        .unwrap();
    e.execute("INSERT INTO small SELECT g FROM generate_series(1, 100) g")
        .unwrap();
    e.execute("DELETE FROM small WHERE id <= 50").unwrap();
}

#[test]
fn tick_vacuums_only_over_threshold_tables() {
    let mut e = Engine::new();
    if !e.mvcc_inplace() {
        return; // gate-off: no tombstones exist.
    }
    e.set_autovacuum_inline(false);
    setup_over_and_under(&mut e);
    e.execute("DELETE FROM big WHERE id <= 1200").unwrap();
    assert!(
        dead(&mut e, "big") >= 1200,
        "inline off: the DELETE must NOT have vacuumed"
    );
    let small_dead_before = dead(&mut e, "small");
    assert!(small_dead_before > 0, "small has tombstones");

    let vacuumed = e.autovacuum_tick();
    assert_eq!(vacuumed, 1, "exactly the over-threshold table");
    assert_eq!(dead(&mut e, "big"), 0, "tick reclaimed big");
    assert_eq!(
        dead(&mut e, "small"),
        small_dead_before,
        "under-threshold table untouched"
    );
    assert_eq!(one(&mut e, "SELECT count(*) FROM big"), 300);
    // Nothing left over threshold — next tick is a no-op.
    assert_eq!(e.autovacuum_tick(), 0);
}

#[test]
fn inline_default_still_vacuums_at_statement_exit() {
    // The embedded default (inline ON) keeps the pre-r173 behavior:
    // the crossing DML statement itself reclaims.
    let mut e = Engine::new();
    if !e.mvcc_inplace() {
        return;
    }
    e.execute("CREATE TABLE t(id INT PRIMARY KEY)").unwrap();
    e.execute("INSERT INTO t SELECT g FROM generate_series(1, 1500) g")
        .unwrap();
    e.execute("DELETE FROM t WHERE id <= 1200").unwrap();
    assert_eq!(
        dead(&mut e, "t"),
        0,
        "inline autovacuum must fire at statement exit"
    );
}

#[test]
fn tick_noop_inside_explicit_transaction() {
    let mut e = Engine::new();
    if !e.mvcc_inplace() {
        return;
    }
    e.set_autovacuum_inline(false);
    e.execute("CREATE TABLE t(id INT PRIMARY KEY)").unwrap();
    e.execute("INSERT INTO t SELECT g FROM generate_series(1, 1500) g")
        .unwrap();
    e.execute("DELETE FROM t WHERE id <= 1200").unwrap();
    e.execute("BEGIN").unwrap();
    e.execute("DELETE FROM t WHERE id = 1300").unwrap();
    assert_eq!(e.autovacuum_tick(), 0, "no vacuum while a tx is open");
    e.execute("ROLLBACK").unwrap();
    assert_eq!(one(&mut e, "SELECT count(*) FROM t"), 300);
    assert_eq!(e.autovacuum_tick(), 1, "backlog picked up after the tx");
    assert_eq!(dead(&mut e, "t"), 0);
}
