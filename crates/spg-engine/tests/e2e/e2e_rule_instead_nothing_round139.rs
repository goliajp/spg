//! v7.39 (read01 round 139, CREATE RULE — Phase 1) — an unconditional
//! `DO INSTEAD NOTHING` rule turns INSERT / UPDATE / DELETE on its target into a
//! no-op. Before this round SPG accepted `CREATE RULE` and then silently ignored
//! it (a `DO INSTEAD NOTHING` rule did not block anything — the DML still ran).
//! Locked byte-identical against PG 18.4: the statement affects zero rows and,
//! when it carries RETURNING, PG rejects it outright (the rows can never be
//! produced). Every other rule form is refused at CREATE RULE time (Phase 2).

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE RULE r_ins AS ON INSERT TO t DO INSTEAD NOTHING")
        .unwrap();
    e.execute("CREATE RULE r_upd AS ON UPDATE TO t DO INSTEAD NOTHING")
        .unwrap();
    e.execute("CREATE RULE r_del AS ON DELETE TO t DO INSTEAD NOTHING")
        .unwrap();
}

fn pairs(e: &mut Engine) -> Vec<(i32, i32)> {
    match e.execute("SELECT id, v FROM t ORDER BY id").unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| match (&r.values[0], &r.values[1]) {
                (spg_storage::Value::Int(a), spg_storage::Value::Int(b)) => (*a, *b),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn affected(r: QueryResult) -> usize {
    match r {
        QueryResult::CommandOk { affected, .. } => affected,
        other => panic!("{other:?}"),
    }
}

#[test]
fn delete_is_blocked() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(e.execute("DELETE FROM t WHERE id = 1").unwrap()),
        0
    );
    assert_eq!(affected(e.execute("DELETE FROM t").unwrap()), 0);
    assert_eq!(pairs(&mut e), vec![(1, 10), (2, 20)]);
}

#[test]
fn update_is_blocked() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(e.execute("UPDATE t SET v = 99 WHERE id = 2").unwrap()),
        0
    );
    assert_eq!(affected(e.execute("UPDATE t SET v = 0").unwrap()), 0);
    assert_eq!(pairs(&mut e), vec![(1, 10), (2, 20)]);
}

#[test]
fn insert_is_blocked() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(e.execute("INSERT INTO t VALUES (3, 30)").unwrap()),
        0
    );
    assert_eq!(
        affected(e.execute("INSERT INTO t VALUES (4, 40), (5, 50)").unwrap()),
        0
    );
    assert_eq!(pairs(&mut e), vec![(1, 10), (2, 20)]);
}

#[test]
fn blocked_returning_is_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    for (sql, op) in [
        ("DELETE FROM t RETURNING id", "DELETE"),
        ("UPDATE t SET v = 1 RETURNING id", "UPDATE"),
        ("INSERT INTO t VALUES (9, 9) RETURNING id", "INSERT"),
    ] {
        let m = match e.execute(sql) {
            Err(x) => format!("{x}"),
            Ok(_) => panic!("expected error for {sql}"),
        };
        assert!(
            m.contains(&format!("cannot perform {op} RETURNING on relation \"t\"")),
            "{m}"
        );
        assert!(
            m.contains(&format!(
                "You need an unconditional ON {op} DO INSTEAD rule with a RETURNING clause"
            )),
            "{m}"
        );
    }
    // The rejected statements did not touch the table.
    assert_eq!(pairs(&mut e), vec![(1, 10), (2, 20)]);
}

#[test]
fn unsupported_rule_forms_are_rejected_not_swallowed() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u(id int, v int)").unwrap();
    e.execute("CREATE TABLE aud(id int)").unwrap();
    // Conditional DO INSTEAD <command> — the one remaining unsupported form.
    let m = match e.execute(
        "CREATE RULE ra AS ON INSERT TO u WHERE NEW.v < 0 DO INSTEAD INSERT INTO aud VALUES (1)",
    ) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(
        m.contains("conditional (WHERE) DO INSTEAD <command> rules are not yet implemented"),
        "{m}"
    );
    // Unconditional DO INSTEAD <command> is supported since round 142; the
    // conditional DO INSTEAD NOTHING form since round 141.
    // ON SELECT — use CREATE VIEW.
    let m = match e.execute("CREATE RULE rs AS ON SELECT TO u DO INSTEAD NOTHING") {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("ON SELECT rules are not supported"), "{m}");
    // A rule on a non-existent relation is refused.
    let m = match e.execute("CREATE RULE rn AS ON DELETE TO nope DO INSTEAD NOTHING") {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("relation \"nope\" does not exist"), "{m}");
}

#[test]
fn drop_rule_reenables_dml() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        affected(e.execute("DELETE FROM t WHERE id = 1").unwrap()),
        0
    );
    e.execute("DROP RULE r_del ON t").unwrap();
    assert_eq!(
        affected(e.execute("DELETE FROM t WHERE id = 1").unwrap()),
        1
    );
    assert_eq!(pairs(&mut e), vec![(2, 20)]);
    // IF EXISTS on a missing rule is a no-op; a bare drop of a missing rule errors.
    e.execute("DROP RULE IF EXISTS ghost ON t").unwrap();
    assert!(e.execute("DROP RULE ghost ON t").is_err());
}
