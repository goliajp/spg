//! v7.37.19 (19.11) — GROUPS frame mode.

use spg_engine::{Engine, QueryResult};
use spg_storage::Value;

fn ddl(e: &mut Engine, sql: &str) {
    e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
}

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("Rows");
    };
    rows.into_iter().map(|r| r.values).collect()
}

#[test]
fn groups_unbounded_preceding_and_current_row_matches_range() {
    // GROUPS UNBOUNDED PRECEDING AND CURRENT ROW behaves identically
    // to RANGE UNBOUNDED PRECEDING AND CURRENT ROW — both consult
    // the peer-group at the current row.
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT, v INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1, 10), (2, 10), (3, 20), (4, 30)");

    let groups = rows(
        &mut e,
        "SELECT id, SUM(v) OVER (ORDER BY v GROUPS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
         FROM t ORDER BY id",
    );
    let range = rows(
        &mut e,
        "SELECT id, SUM(v) OVER (ORDER BY v RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) \
         FROM t ORDER BY id",
    );
    assert_eq!(groups, range);
}

#[test]
fn groups_current_row_to_unbounded_following_matches_range() {
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT, v INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1, 10), (2, 10), (3, 20), (4, 30)");

    let groups = rows(
        &mut e,
        "SELECT id, SUM(v) OVER (ORDER BY v GROUPS BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) \
         FROM t ORDER BY id",
    );
    let range = rows(
        &mut e,
        "SELECT id, SUM(v) OVER (ORDER BY v RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) \
         FROM t ORDER BY id",
    );
    assert_eq!(groups, range);
}

#[test]
fn groups_offset_bounds_supported() {
    // v7.37 D — GROUPS with an integer offset is now supported (peer-group
    // counted frame). A lone row's frame is just its own group. PG18: 10.
    let mut e = Engine::new();
    ddl(&mut e, "CREATE TABLE t (id INT, v INT)");
    ddl(&mut e, "INSERT INTO t VALUES (1, 10)");
    let out = rows(
        &mut e,
        "SELECT SUM(v) OVER (ORDER BY v GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t",
    );
    assert_eq!(out, vec![vec![Value::Float(10.0)]]);
}
