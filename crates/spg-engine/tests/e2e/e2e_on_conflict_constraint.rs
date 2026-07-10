//! v7.37.17 (17.6 siblings) — ON CONFLICT ON CONSTRAINT <name>,
//! the pg_dump conflict-target form.

use spg_engine::{Engine, QueryResult};

fn ints(e: &mut Engine, sql: &str) -> Vec<i64> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter()
        .map(|row| match &row.values[0] {
            spg_storage::Value::Int(n) => i64::from(*n),
            spg_storage::Value::BigInt(n) => *n,
            other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

#[test]
fn do_nothing_via_pkey_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE u (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO u VALUES (1, 10)").unwrap();
    // The synthetic PK constraint is named u_pkey.
    e.execute("INSERT INTO u VALUES (1, 99) ON CONFLICT ON CONSTRAINT u_pkey DO NOTHING")
        .unwrap();
    assert_eq!(ints(&mut e, "SELECT v FROM u WHERE id = 1"), [10]);
}

#[test]
fn do_update_via_pkey_name() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE w (id INT PRIMARY KEY, v INT)")
        .unwrap();
    e.execute("INSERT INTO w VALUES (1, 10)").unwrap();
    e.execute(
        "INSERT INTO w VALUES (1, 99) ON CONFLICT ON CONSTRAINT w_pkey \
         DO UPDATE SET v = EXCLUDED.v",
    )
    .unwrap();
    assert_eq!(ints(&mut e, "SELECT v FROM w WHERE id = 1"), [99]);
}

#[test]
fn unknown_constraint_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE z (id INT PRIMARY KEY)").unwrap();
    let err = e
        .execute("INSERT INTO z VALUES (1) ON CONFLICT ON CONSTRAINT nope DO NOTHING")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("nope"), "unexpected error: {msg}");
}
