//! v7.39 (read01 round 143, CREATE RULE — Phase 4) — `pg_catalog.pg_rules`
//! introspection (one row per catalogued rule: schemaname / tablename /
//! rulename / definition, the same fidelity level as `pg_views.definition`)
//! and `CREATE OR REPLACE RULE` (swaps a same-(name, table) rule in place;
//! a duplicate without OR REPLACE errors, as PG does).

use spg_engine::{Engine, QueryResult};

fn texts(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Text(s) => s.to_string(),
                        other => panic!("non-text {other:?}"),
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
fn pg_rules_lists_every_form() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE rt(id int, v int)").unwrap();
    e.execute("CREATE TABLE raud(id int, v int)").unwrap();
    e.execute("CREATE RULE r_block AS ON DELETE TO rt DO INSTEAD NOTHING").unwrap();
    e.execute("CREATE RULE r_cond AS ON UPDATE TO rt WHERE NEW.v < 0 DO INSTEAD NOTHING")
        .unwrap();
    e.execute("CREATE RULE r_also AS ON INSERT TO rt DO ALSO INSERT INTO raud VALUES (NEW.id, NEW.v)")
        .unwrap();
    let rows = texts(
        &mut e,
        "SELECT schemaname, tablename, rulename, definition FROM pg_rules ORDER BY rulename",
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert_eq!(rows[0][..3], ["public", "rt", "r_also"].map(String::from));
    // DO ALSO deparses as bare `DO <command>` (ALSO is the default), like PG.
    assert!(rows[0][3].starts_with("CREATE RULE r_also AS ON INSERT TO public.rt DO INSERT INTO raud"), "{}", rows[0][3]);
    assert_eq!(rows[1][..3], ["public", "rt", "r_block"].map(String::from));
    assert_eq!(rows[1][3], "CREATE RULE r_block AS ON DELETE TO public.rt DO INSTEAD NOTHING;");
    assert_eq!(rows[2][..3], ["public", "rt", "r_cond"].map(String::from));
    assert_eq!(
        rows[2][3],
        "CREATE RULE r_cond AS ON UPDATE TO public.rt WHERE (new.v < 0) DO INSTEAD NOTHING;"
    );
}

#[test]
fn or_replace_swaps_rule_in_place() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int, v int)").unwrap();
    e.execute("INSERT INTO t VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE RULE r AS ON DELETE TO t DO INSTEAD NOTHING").unwrap();
    assert_eq!(affected(&mut e, "DELETE FROM t WHERE id = 1"), 0);
    // Replace the unconditional blocker with a conditional one.
    e.execute("CREATE OR REPLACE RULE r AS ON DELETE TO t WHERE OLD.v > 15 DO INSTEAD NOTHING")
        .unwrap();
    // Still one catalogued rule, with the new definition.
    let rows = texts(&mut e, "SELECT rulename, definition FROM pg_rules ORDER BY rulename");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0][1].contains("WHERE (old.v > 15)"), "{}", rows[0][1]);
    // And the new behavior applies: id=1 (v=10) now deletable, id=2 blocked.
    assert_eq!(affected(&mut e, "DELETE FROM t"), 1);
    match e.execute("SELECT id FROM t").unwrap() {
        QueryResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn duplicate_without_or_replace_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("CREATE RULE r AS ON DELETE TO t DO INSTEAD NOTHING").unwrap();
    let m = match e.execute("CREATE RULE r AS ON DELETE TO t DO INSTEAD NOTHING") {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("rule \"r\" for relation \"t\" already exists"), "{m}");
}

#[test]
fn drop_rule_removes_pg_rules_row() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("CREATE RULE r AS ON DELETE TO t DO INSTEAD NOTHING").unwrap();
    assert_eq!(texts(&mut e, "SELECT rulename FROM pg_rules").len(), 1);
    e.execute("DROP RULE r ON t").unwrap();
    assert_eq!(texts(&mut e, "SELECT rulename FROM pg_rules").len(), 0);
}
