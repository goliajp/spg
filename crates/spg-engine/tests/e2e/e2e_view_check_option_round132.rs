//! v7.39 (read01 round 132, PG-feature — WITH CHECK OPTION) — an auto-updatable
//! view created `WITH [LOCAL|CASCADED] CHECK OPTION` rejects a write whose
//! resulting row fails the view's WHERE (SQLSTATE 44000). Locked byte-identical
//! against PG 18.4: only a definite TRUE passes (NULL / FALSE fail, mirroring
//! row visibility); the error carries `DETAIL: Failing row contains (…).`.
//!
//! A view without the option enforces nothing (regression guard). The wire
//! SQLSTATE 44000 mapping is covered by a pgwire unit test.

use spg_engine::{Engine, QueryResult};

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE vt(id int, x int)").unwrap();
    e.execute("CREATE VIEW vv AS SELECT * FROM vt WHERE x > 0 WITH CHECK OPTION")
        .unwrap();
    e.execute("CREATE VIEW vn AS SELECT * FROM vt WHERE x > 0")
        .unwrap();
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Err(x) => format!("{x}"),
        Ok(_) => panic!("{sql}: expected error, got Ok"),
    }
}

fn count(e: &mut Engine) -> i64 {
    match e.execute("SELECT count(*) FROM vt").unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::BigInt(n) => *n,
            other => panic!("count: {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_violating_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    let msg = err_of(&mut e, "INSERT INTO vv VALUES(1,-1)");
    assert!(
        msg.contains("new row violates check option for view \"vv\""),
        "{msg}"
    );
    assert!(msg.contains("Failing row contains (1, -1)"), "{msg}");
    assert_eq!(count(&mut e), 0, "violating insert must not land");
}

#[test]
fn insert_satisfying_ok() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO vv VALUES(2,5)").unwrap();
    assert_eq!(count(&mut e), 1);
}

#[test]
fn insert_null_qual_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    // WHERE `NULL > 0` is NULL, not TRUE → rejected (row not visible).
    let msg = err_of(&mut e, "INSERT INTO vv VALUES(3,NULL)");
    assert!(
        msg.contains("new row violates check option for view \"vv\""),
        "{msg}"
    );
    assert_eq!(count(&mut e), 0);
}

#[test]
fn update_to_violate_rejected() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO vv VALUES(1,5)").unwrap();
    let msg = err_of(&mut e, "UPDATE vv SET x=-9 WHERE id=1");
    assert!(
        msg.contains("new row violates check option for view \"vv\""),
        "{msg}"
    );
    assert!(msg.contains("Failing row contains (1, -9)"), "{msg}");
    // Row unchanged.
    match e.execute("SELECT x FROM vt WHERE id=1").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::Int(5));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn update_staying_valid_ok() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("INSERT INTO vv VALUES(1,5)").unwrap();
    e.execute("UPDATE vv SET x=8 WHERE id=1").unwrap();
    match e.execute("SELECT x FROM vt WHERE id=1").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows[0].values[0], spg_storage::Value::Int(8));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn view_without_option_enforces_nothing() {
    let mut e = Engine::new();
    setup(&mut e);
    // vn has no WITH CHECK OPTION → a violating insert succeeds (PG-faithful).
    e.execute("INSERT INTO vn VALUES(2,-3)").unwrap();
    assert_eq!(count(&mut e), 1);
}

#[test]
fn information_schema_check_option() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute("CREATE VIEW vl AS SELECT * FROM vt WHERE x > 0 WITH LOCAL CHECK OPTION")
        .unwrap();
    // PG: bare WITH CHECK OPTION → CASCADED, WITH LOCAL → LOCAL, none → NONE.
    let want = [("vv", "CASCADED"), ("vl", "LOCAL"), ("vn", "NONE")];
    for (name, opt) in want {
        let sql = format!(
            "SELECT check_option FROM information_schema.views WHERE table_name='{name}'"
        );
        match e.execute(&sql).unwrap() {
            QueryResult::Rows { rows, .. } => {
                assert_eq!(
                    spg_engine::eval::value_to_text(&rows[0].values[0]),
                    opt,
                    "check_option for {name}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
