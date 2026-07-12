//! v7.39 — ALTER COLUMN TYPE data-conversion semantics (assignment
//! cast without USING; PG-phrased refusal for narrowing casts).
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};


#[test]
fn alter_type_to_text_rewrites_existing_rows() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE att (a INT)").unwrap();
    e.execute("INSERT INTO att VALUES (5),(42)").unwrap();
    e.execute("ALTER TABLE att ALTER COLUMN a TYPE TEXT")
        .expect("INT -> TEXT is an automatic assignment cast in PG");
    let QueryResult::Rows { rows, .. } =
        e.execute("SELECT a FROM att ORDER BY a").unwrap()
    else {
        panic!("rows")
    };
    let got: Vec<_> = rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        got,
        vec![
            spg_storage::Value::Text("42".into()),
            spg_storage::Value::Text("5".into())
        ],
        "values rewritten through the output function; text sort order"
    );
}

#[test]
fn alter_type_narrowing_without_using_refuses_with_pg_phrasing() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE att2 (t TEXT)").unwrap();
    e.execute("INSERT INTO att2 VALUES ('xyz')").unwrap();
    let err = e
        .execute("ALTER TABLE att2 ALTER COLUMN t TYPE INT")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot be cast automatically"),
        "got: {err}"
    );
    // USING makes the conversion explicit and succeeds for castable data.
    e.execute("UPDATE att2 SET t = '7'").unwrap();
    e.execute("ALTER TABLE att2 ALTER COLUMN t TYPE INT USING t::integer")
        .expect("USING t::integer converts");
}
