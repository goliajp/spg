//! v7.39 (read01 round 142, CREATE RULE — Phase 3b) — unconditional
//! `DO INSTEAD <command>` rules: the original DML never runs; the command runs
//! once per affected row with NEW / OLD bound. Locked byte-identical against
//! PG 18.4 (probed live): INSERT reports the SOURCE row count in its tag while
//! UPDATE / DELETE report 0; NEW on a redirected INSERT sees defaults; the
//! classic soft-delete rewrite (`ON DELETE ... DO INSTEAD UPDATE <same table>`)
//! works; an outer RETURNING is rejected with PG's exact error.

use spg_engine::{Engine, QueryResult};

fn rows_i(e: &mut Engine, sql: &str) -> Vec<Vec<i32>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Int(a) => *a,
                        other => panic!("non-int {other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn affected(e: &mut Engine, sql: &str) -> usize {
    match e.execute(sql).unwrap() {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_redirect_multi_row_source_count_tag() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("CREATE TABLE redir(id int, v int)").unwrap();
    e.execute(
        "CREATE RULE ri AS ON INSERT TO t DO INSTEAD INSERT INTO redir VALUES (NEW.id, NEW.v*10)",
    )
    .unwrap();
    // PG tags the source row count even though nothing lands in t.
    assert_eq!(affected(&mut e, "INSERT INTO t VALUES (1,10),(2,20)"), 2);
    assert_eq!(rows_i(&mut e, "SELECT count(*)::int FROM t"), vec![vec![0]]);
    assert_eq!(
        rows_i(&mut e, "SELECT id, v FROM redir ORDER BY id"),
        vec![vec![1, 100], vec![2, 200]]
    );
}

#[test]
fn insert_redirect_new_sees_defaults() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int, v int DEFAULT 5)")
        .unwrap();
    e.execute("CREATE TABLE redir(id int, v int)").unwrap();
    e.execute(
        "CREATE RULE rd AS ON INSERT TO d DO INSTEAD INSERT INTO redir VALUES (NEW.id, NEW.v)",
    )
    .unwrap();
    assert_eq!(affected(&mut e, "INSERT INTO d(id) VALUES (1)"), 1);
    assert_eq!(rows_i(&mut e, "SELECT id, v FROM redir"), vec![vec![1, 5]]);
    assert_eq!(rows_i(&mut e, "SELECT count(*)::int FROM d"), vec![vec![0]]);
}

#[test]
fn update_instead_logs_old_and_new() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("CREATE TABLE log(id int, nv int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,1),(2,2),(3,3)").unwrap();
    e.execute("CREATE RULE ru AS ON UPDATE TO t DO INSTEAD INSERT INTO log VALUES (OLD.id, NEW.v)")
        .unwrap();
    // PG reports UPDATE 0; the log captures per-row OLD.id + derived NEW.v.
    assert_eq!(affected(&mut e, "UPDATE t SET v = v * 10 WHERE id >= 2"), 0);
    assert_eq!(
        rows_i(&mut e, "SELECT id, nv FROM log ORDER BY id"),
        vec![vec![2, 20], vec![3, 30]]
    );
    // The base table is untouched.
    assert_eq!(
        rows_i(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![vec![1, 1], vec![2, 2], vec![3, 3]]
    );
}

#[test]
fn delete_soft_delete_same_table() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE s(id int, dead int DEFAULT 0)")
        .unwrap();
    e.execute("INSERT INTO s VALUES (1,0),(2,0)").unwrap();
    e.execute(
        "CREATE RULE rd AS ON DELETE TO s DO INSTEAD UPDATE s SET dead = 1 WHERE id = OLD.id",
    )
    .unwrap();
    assert_eq!(affected(&mut e, "DELETE FROM s WHERE id = 1"), 0);
    assert_eq!(
        rows_i(&mut e, "SELECT id, dead FROM s ORDER BY id"),
        vec![vec![1, 1], vec![2, 0]]
    );
}

#[test]
fn outer_returning_rejected_on_all_three() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("CREATE TABLE x(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10)").unwrap();
    e.execute("CREATE RULE ri AS ON INSERT TO t DO INSTEAD INSERT INTO x VALUES (NEW.id, NEW.v)")
        .unwrap();
    e.execute("CREATE RULE ru AS ON UPDATE TO t DO INSTEAD INSERT INTO x VALUES (OLD.id, NEW.v)")
        .unwrap();
    e.execute("CREATE RULE rd AS ON DELETE TO t DO INSTEAD INSERT INTO x VALUES (OLD.id, OLD.v)")
        .unwrap();
    for (sql, op) in [
        ("INSERT INTO t VALUES (9,9) RETURNING id", "INSERT"),
        ("UPDATE t SET v = 1 RETURNING id", "UPDATE"),
        ("DELETE FROM t RETURNING id", "DELETE"),
    ] {
        let m = match e.execute(sql) {
            Err(x) => format!("{x}"),
            Ok(_) => panic!("expected error for {sql}"),
        };
        assert!(
            m.contains(&format!("cannot perform {op} RETURNING on relation \"t\"")),
            "{m}"
        );
    }
}

#[test]
fn rule_cycle_errors_not_stack_overflow() {
    // A DO INSTEAD command targeting a table whose rule points back must error
    // via the recursion cap, not blow the stack.
    let mut e = Engine::new();
    e.execute("CREATE TABLE a(id int)").unwrap();
    e.execute("CREATE TABLE b(id int)").unwrap();
    e.execute("CREATE RULE rab AS ON INSERT TO a DO INSTEAD INSERT INTO b VALUES (NEW.id)")
        .unwrap();
    e.execute("CREATE RULE rba AS ON INSERT TO b DO INSTEAD INSERT INTO a VALUES (NEW.id)")
        .unwrap();
    assert!(e.execute("INSERT INTO a VALUES (1)").is_err());
}
