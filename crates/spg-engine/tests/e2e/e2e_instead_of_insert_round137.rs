//! v7.39 (read01 round 137, INSTEAD OF triggers — Phase 1: INSERT) — a view
//! carrying an `INSTEAD OF INSERT` trigger fires the trigger per row instead of
//! the auto-updatable redirect; the plpgsql function body does the real write
//! (e.g. INSERT INTO the base table). This makes even a non-auto-updatable view
//! (computed columns / joins) writable. Locked byte-identical against PG 18.4.
//!
//! Also covers the timing↔target rule: INSTEAD OF only on views, BEFORE/AFTER
//! only on tables. RETURNING through an INSTEAD OF trigger is a Phase-2 residual
//! (errors honestly for now).

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE base(id int, v int)").unwrap();
    // A non-auto-updatable view (has a computed column).
    e.execute("CREATE VIEW jv AS SELECT id, v, v*2 AS dbl FROM base").unwrap();
    e.execute(
        "CREATE FUNCTION jv_ins() RETURNS trigger AS $x$ BEGIN \
         INSERT INTO base(id,v) VALUES(NEW.id, NEW.v); RETURN NEW; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE TRIGGER jv_ins_t INSTEAD OF INSERT ON jv FOR EACH ROW EXECUTE FUNCTION jv_ins()",
    )
    .unwrap();
}

fn base_pairs(e: &mut Engine) -> Vec<(i32, i32)> {
    match e.execute("SELECT id, v FROM base ORDER BY id").unwrap() {
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

#[test]
fn instead_of_insert_fires_and_writes_base() {
    let mut e = Engine::new();
    setup(&mut e);
    // Multi-row INSERT through the non-updatable view fires the trigger per row.
    match e.execute("INSERT INTO jv(id,v) VALUES(1,10),(2,20)").unwrap() {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(base_pairs(&mut e), vec![(1, 10), (2, 20)]);
}

#[test]
fn instead_of_insert_column_order() {
    let mut e = Engine::new();
    setup(&mut e);
    // Column list in a different order maps NEW.id / NEW.v correctly.
    e.execute("INSERT INTO jv(v,id) VALUES(30,3)").unwrap();
    assert_eq!(base_pairs(&mut e), vec![(3, 30)]);
}

#[test]
fn instead_of_insert_positional() {
    let mut e = Engine::new();
    setup(&mut e);
    // Positional insert (id, v, dbl) — only id/v are used by the trigger body.
    e.execute("INSERT INTO jv VALUES(4,40,999)").unwrap();
    assert_eq!(base_pairs(&mut e), vec![(4, 40)]);
}

#[test]
fn instead_of_on_table_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t(id int)").unwrap();
    e.execute("CREATE FUNCTION f() RETURNS trigger AS $x$ BEGIN RETURN NEW; END; $x$ LANGUAGE plpgsql")
        .unwrap();
    let m = match e.execute("CREATE TRIGGER x INSTEAD OF INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()") {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("\"t\" is a table"), "{m}");
    assert!(m.contains("Tables cannot have INSTEAD OF triggers"), "{m}");
}

#[test]
fn before_on_view_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base(id int, v int)").unwrap();
    e.execute("CREATE VIEW jv AS SELECT id, v FROM base").unwrap();
    e.execute("CREATE FUNCTION f() RETURNS trigger AS $x$ BEGIN RETURN NEW; END; $x$ LANGUAGE plpgsql")
        .unwrap();
    let m = match e.execute("CREATE TRIGGER y BEFORE INSERT ON jv FOR EACH ROW EXECUTE FUNCTION f()") {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("\"jv\" is a view"), "{m}");
}
