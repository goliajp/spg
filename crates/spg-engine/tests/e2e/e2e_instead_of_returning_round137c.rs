//! v7.39 (read01 round 137, INSTEAD OF triggers — Phase 3: RETURNING) — a
//! RETURNING clause on a write through an INSTEAD OF view projects the row the
//! trigger returned: NEW for INSERT / UPDATE, OLD for DELETE. Locked
//! byte-identical against PG 18.4. Completes the INSTEAD OF triggers epic's
//! row-DML surface.

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE base(id int, v int)").unwrap();
    e.execute("INSERT INTO base VALUES(2,20)").unwrap();
    e.execute("CREATE VIEW jv AS SELECT id, v, v*2 AS dbl FROM base").unwrap();
    e.execute(
        "CREATE FUNCTION jv_ins() RETURNS trigger AS $x$ BEGIN \
         INSERT INTO base(id,v) VALUES(NEW.id,NEW.v); RETURN NEW; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute("CREATE TRIGGER ti INSTEAD OF INSERT ON jv FOR EACH ROW EXECUTE FUNCTION jv_ins()")
        .unwrap();
    e.execute(
        "CREATE FUNCTION jv_upd() RETURNS trigger AS $x$ BEGIN \
         UPDATE base SET v=NEW.v WHERE id=OLD.id; RETURN NEW; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute("CREATE TRIGGER tu INSTEAD OF UPDATE ON jv FOR EACH ROW EXECUTE FUNCTION jv_upd()")
        .unwrap();
    e.execute(
        "CREATE FUNCTION jv_del() RETURNS trigger AS $x$ BEGIN \
         DELETE FROM base WHERE id=OLD.id; RETURN OLD; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute("CREATE TRIGGER td INSTEAD OF DELETE ON jv FOR EACH ROW EXECUTE FUNCTION jv_del()")
        .unwrap();
}

fn row1(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(|v| match v {
                spg_storage::Value::Null => "NULL".to_string(),
                v => spg_engine::eval::value_to_text(v),
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn cols(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { columns, .. } => columns.iter().map(|c| c.name.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn insert_returning_projects_new() {
    let mut e = Engine::new();
    setup(&mut e);
    // NEW = (id=1, v=10, dbl=NULL) — dbl is not computed for the trigger's NEW.
    assert_eq!(
        row1(&mut e, "INSERT INTO jv(id,v) VALUES(1,10) RETURNING id, v, dbl"),
        vec!["1", "10", "NULL"]
    );
}

#[test]
fn update_returning_projects_new() {
    let mut e = Engine::new();
    setup(&mut e);
    // OLD = (2,20,40); NEW = OLD with v=99 → (2,99,40). RETURNING projects NEW.
    assert_eq!(
        row1(&mut e, "UPDATE jv SET v=99 WHERE id=2 RETURNING id, v, dbl"),
        vec!["2", "99", "40"]
    );
}

#[test]
fn delete_returning_projects_old() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO jv(id,v) VALUES(1,10)").unwrap();
    // OLD row for id=1 = (1,10,20); RETURNING id,v → (1,10).
    assert_eq!(
        row1(&mut e, "DELETE FROM jv WHERE id=1 RETURNING id, v"),
        vec!["1", "10"]
    );
}

#[test]
fn returning_column_names() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        cols(&mut e, "INSERT INTO jv(id,v) VALUES(5,50) RETURNING id, v, dbl"),
        vec!["id", "v", "dbl"]
    );
}
