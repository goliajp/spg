//! v7.39 (read01 round 140, CREATE RULE — Phase 2) — `DO ALSO <command>` rules.
//! The original DML runs, and additionally each rule's command fires once per
//! affected row with NEW / OLD bound to that row (post-image for NEW, so
//! defaults / sequences are reflected). Conditional `WHERE` filters per row.
//! Locked byte-identical against PG 18.4 (probed live: the combined
//! INSERT/UPDATE/DELETE audit ends up as {1I10, 1U99, 2D20, 2I20}).

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

fn aud_rows(e: &mut Engine) -> Vec<(i32, String, i32)> {
    match e
        .execute("SELECT id, op, v FROM aud ORDER BY id, op")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match (&r.values[0], &r.values[1], &r.values[2]) {
                (
                    spg_storage::Value::Int(a),
                    spg_storage::Value::Text(op),
                    spg_storage::Value::Int(v),
                ) => (*a, op.to_string(), *v),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("CREATE TABLE aud(id int, op text, v int)")
        .unwrap();
    e.execute(
        "CREATE RULE r_ins AS ON INSERT TO t DO ALSO INSERT INTO aud VALUES (NEW.id, 'I', NEW.v)",
    )
    .unwrap();
    e.execute(
        "CREATE RULE r_upd AS ON UPDATE TO t DO ALSO INSERT INTO aud VALUES (OLD.id, 'U', NEW.v)",
    )
    .unwrap();
    e.execute(
        "CREATE RULE r_del AS ON DELETE TO t DO ALSO INSERT INTO aud VALUES (OLD.id, 'D', OLD.v)",
    )
    .unwrap();
}

#[test]
fn combined_audit_matches_pg() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    e.execute("UPDATE t SET v = 99 WHERE id = 1").unwrap();
    e.execute("DELETE FROM t WHERE id = 2").unwrap();
    // The base table reflects the real writes.
    assert_eq!(
        rows_i(&mut e, "SELECT id, v FROM t ORDER BY id"),
        vec![vec![1, 99]]
    );
    // The audit trail is the per-row rule side effects, in PG's order.
    assert_eq!(
        aud_rows(&mut e),
        vec![
            (1, "I".to_string(), 10),
            (1, "U".to_string(), 99),
            (2, "D".to_string(), 20),
            (2, "I".to_string(), 20),
        ]
    );
}

#[test]
fn insert_new_reflects_default_post_image() {
    // NEW in a DO ALSO INSERT sees the DEFAULT-filled column, not the raw VALUES.
    let mut e = Engine::new();
    e.execute("CREATE TABLE d(id int, v int DEFAULT 5)")
        .unwrap();
    e.execute("CREATE TABLE daud(id int, v int)").unwrap();
    e.execute("CREATE RULE rd AS ON INSERT TO d DO ALSO INSERT INTO daud VALUES (NEW.id, NEW.v)")
        .unwrap();
    e.execute("INSERT INTO d(id) VALUES (1)").unwrap();
    assert_eq!(rows_i(&mut e, "SELECT id, v FROM daud"), vec![vec![1, 5]]);
}

#[test]
fn conditional_where_filters_per_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("CREATE TABLE aud(id int, v int)").unwrap();
    e.execute("CREATE RULE r AS ON INSERT TO t WHERE NEW.v > 15 DO ALSO INSERT INTO aud VALUES (NEW.id, NEW.v)")
        .unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20),(3,30)")
        .unwrap();
    assert_eq!(
        rows_i(&mut e, "SELECT id, v FROM aud ORDER BY id"),
        vec![vec![2, 20], vec![3, 30]]
    );
}

#[test]
fn do_also_coexists_with_returning() {
    let mut e = Engine::new();
    setup(&mut e);
    // RETURNING on the base INSERT still projects; the rule also fires.
    let ret = rows_i(&mut e, "INSERT INTO t VALUES (7,70) RETURNING id, v");
    assert_eq!(ret, vec![vec![7, 70]]);
    assert_eq!(aud_rows(&mut e), vec![(7, "I".to_string(), 70)]);
}

#[test]
fn multi_command_do_also() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("CREATE TABLE a(id int)").unwrap();
    e.execute("CREATE TABLE b(id int)").unwrap();
    e.execute(
        "CREATE RULE r AS ON INSERT TO t DO ALSO (INSERT INTO a VALUES (NEW.id); INSERT INTO b VALUES (NEW.id))",
    )
    .unwrap();
    e.execute("INSERT INTO t VALUES (5,50)").unwrap();
    assert_eq!(rows_i(&mut e, "SELECT id FROM a"), vec![vec![5]]);
    assert_eq!(rows_i(&mut e, "SELECT id FROM b"), vec![vec![5]]);
}

#[test]
fn conditional_do_instead_command_splits_the_statement() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("CREATE TABLE r(id int)").unwrap();
    // v7.39 (round 333, V59) — this used to assert the form was REFUSED
    // ("not yet implemented"). It is implemented: the rows the WHERE holds
    // for take the command, the rest are inserted. Measured on PG 18.4 —
    // inserting (1, -1, 2, -2) answers `INSERT 0 2`.
    e.execute(
        "CREATE RULE ri AS ON INSERT TO t WHERE NEW.id < 0 DO INSTEAD INSERT INTO r VALUES (NEW.id)",
    )
    .expect("PG accepts this rule");
    match e
        .execute("INSERT INTO t VALUES (1), (-1), (2), (-2)")
        .unwrap()
    {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(ids(&mut e, "SELECT id FROM t ORDER BY id"), vec![1, 2]);
    assert_eq!(ids(&mut e, "SELECT id FROM r ORDER BY id"), vec![-2, -1]);
}

/// Read a single INT column.
fn ids(e: &mut Engine, sql: &str) -> Vec<i32> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .filter_map(|r| match r.values.first() {
                Some(spg_storage::Value::Int(n)) => Some(*n),
                _ => None,
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}
