//! v7.39 (read01 round 138) — `CREATE TRIGGER … WHEN ( condition )` row-level
//! filter. A BEFORE/AFTER row trigger fires only when its WHEN condition (over
//! NEW / OLD) is a definite TRUE. INSTEAD OF triggers cannot have WHEN. Locked
//! byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

#[test]
fn when_filters_insert_update_delete() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tt(id int, v int)").unwrap();
    e.execute("CREATE TABLE lg(tag text)").unwrap();
    e.execute(
        "CREATE FUNCTION lf() RETURNS trigger AS $x$ BEGIN \
         INSERT INTO lg VALUES(TG_OP); RETURN COALESCE(NEW,OLD); END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    // AFTER INSERT WHEN NEW.v > 10.
    e.execute("CREATE TRIGGER ti AFTER INSERT ON tt FOR EACH ROW WHEN (NEW.v > 10) EXECUTE FUNCTION lf()")
        .unwrap();
    // BEFORE UPDATE WHEN NEW.v > OLD.v (only fires when v grows).
    e.execute("CREATE TRIGGER tu BEFORE UPDATE ON tt FOR EACH ROW WHEN (NEW.v > OLD.v) EXECUTE FUNCTION lf()")
        .unwrap();
    // BEFORE DELETE WHEN OLD.v < 0.
    e.execute("CREATE TRIGGER td BEFORE DELETE ON tt FOR EACH ROW WHEN (OLD.v < 0) EXECUTE FUNCTION lf()")
        .unwrap();

    e.execute("INSERT INTO tt VALUES(1,5),(2,20),(3,50)").unwrap(); // fires for 20,50
    e.execute("UPDATE tt SET v=v+1 WHERE id IN (1,2)").unwrap(); // v grows → 2 fire
    e.execute("UPDATE tt SET v=v-100 WHERE id=3").unwrap(); // v shrinks → 0 fire
    e.execute("INSERT INTO tt VALUES(9,-5)").unwrap(); // v=-5 not >10 → 0 fire
    e.execute("DELETE FROM tt").unwrap(); // OLD.v<0 for id3(-50),id9(-5) → 2 fire

    match e
        .execute("SELECT string_agg(tag, ',' ORDER BY tag) FROM lg")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(
                spg_engine::eval::value_to_text(&rows[0].values[0]),
                "DELETE,DELETE,INSERT,INSERT,UPDATE,UPDATE"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn when_null_condition_does_not_fire() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tt(id int, v int)").unwrap();
    e.execute("CREATE TABLE lg(tag text)").unwrap();
    e.execute(
        "CREATE FUNCTION lf() RETURNS trigger AS $x$ BEGIN \
         INSERT INTO lg VALUES('x'); RETURN NEW; END; $x$ LANGUAGE plpgsql",
    )
    .unwrap();
    e.execute("CREATE TRIGGER ti AFTER INSERT ON tt FOR EACH ROW WHEN (NEW.v > 10) EXECUTE FUNCTION lf()")
        .unwrap();
    // NEW.v NULL → `NULL > 10` is NULL, not TRUE → does not fire.
    e.execute("INSERT INTO tt VALUES(1, NULL)").unwrap();
    match e.execute("SELECT count(*) FROM lg").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::BigInt(0));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn instead_of_with_when_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE base(id int)").unwrap();
    e.execute("CREATE VIEW jv AS SELECT id FROM base").unwrap();
    e.execute("CREATE FUNCTION f() RETURNS trigger AS $x$ BEGIN RETURN NEW; END; $x$ LANGUAGE plpgsql")
        .unwrap();
    let m = match e.execute(
        "CREATE TRIGGER x INSTEAD OF INSERT ON jv FOR EACH ROW WHEN (NEW.id > 0) EXECUTE FUNCTION f()",
    ) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("expected error"),
    };
    assert!(m.contains("INSTEAD OF triggers cannot have WHEN conditions"), "{m}");
}
