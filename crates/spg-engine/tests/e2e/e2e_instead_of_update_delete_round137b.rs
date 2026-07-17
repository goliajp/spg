//! v7.39 (read01 round 137, INSTEAD OF triggers — Phase 2: UPDATE / DELETE) —
//! a view with an INSTEAD OF UPDATE / DELETE trigger scans the view for the rows
//! its WHERE matches (the OLD rows), derives NEW per row from the SET list
//! (UPDATE), and fires the trigger per row; the plpgsql body does the real
//! write. Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE base(id int, v int)").unwrap();
    e.execute("INSERT INTO base VALUES(1,10),(2,20),(3,30)")
        .unwrap();
    e.execute("CREATE VIEW jv AS SELECT id, v, v*2 AS dbl FROM base")
        .unwrap();
    e.execute(
        "CREATE FUNCTION jv_upd() RETURNS trigger AS $x$ BEGIN \
         UPDATE base SET v=NEW.v WHERE id=OLD.id; RETURN NEW; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE TRIGGER jv_upd_t INSTEAD OF UPDATE ON jv FOR EACH ROW EXECUTE FUNCTION jv_upd()",
    )
    .unwrap();
    e.execute(
        "CREATE FUNCTION jv_del() RETURNS trigger AS $x$ BEGIN \
         DELETE FROM base WHERE id=OLD.id; RETURN OLD; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute(
        "CREATE TRIGGER jv_del_t INSTEAD OF DELETE ON jv FOR EACH ROW EXECUTE FUNCTION jv_del()",
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
fn instead_of_update_fires_with_old_and_new() {
    let mut e = Engine::new();
    setup(&mut e);
    // OLD.id=1, NEW.v=99 → the body updates base row 1 to v=99.
    match e.execute("UPDATE jv SET v=99 WHERE id=1").unwrap() {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 1),
        other => panic!("{other:?}"),
    }
    assert_eq!(base_pairs(&mut e), vec![(1, 99), (2, 20), (3, 30)]);
}

#[test]
fn instead_of_update_multi_row_where() {
    let mut e = Engine::new();
    setup(&mut e);
    // WHERE matches two view rows (v>=20): each fires with its own OLD; the SET
    // expression may reference OLD via the view columns.
    match e.execute("UPDATE jv SET v=v+1 WHERE v>=20").unwrap() {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(base_pairs(&mut e), vec![(1, 10), (2, 21), (3, 31)]);
}

#[test]
fn instead_of_delete_fires_per_matching_row() {
    let mut e = Engine::new();
    setup(&mut e);
    // WHERE v>=20 matches view rows for id 2 and 3 → both deleted from base.
    match e.execute("DELETE FROM jv WHERE v >= 20").unwrap() {
        QueryResult::CommandOk { affected, .. } => assert_eq!(affected, 2),
        other => panic!("{other:?}"),
    }
    assert_eq!(base_pairs(&mut e), vec![(1, 10)]);
}

#[test]
fn instead_of_delete_all() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("DELETE FROM jv").unwrap();
    assert_eq!(base_pairs(&mut e), Vec::<(i32, i32)>::new());
}
