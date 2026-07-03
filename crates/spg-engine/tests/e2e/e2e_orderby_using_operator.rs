//! ORDER BY ... USING <op> + OPERATOR(schema.op) explicit
//! operator spelling.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn using_lt_is_asc_gt_is_desc() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ou (v INT)").unwrap();
    e.execute("INSERT INTO ou VALUES (2), (1), (3)").unwrap();
    let got = rows(&mut e, "SELECT v FROM ou ORDER BY v USING <");
    assert_eq!(as_i64(&got[0][0]), 1);
    assert_eq!(as_i64(&got[2][0]), 3);
    let got = rows(&mut e, "SELECT v FROM ou ORDER BY v USING >");
    assert_eq!(as_i64(&got[0][0]), 3);
    // Non-btree operator errors honestly.
    let err = e.execute("SELECT v FROM ou ORDER BY v USING +").unwrap_err();
    assert!(format!("{err:?}").contains("btree"));
}

#[test]
fn operator_syntax_dispatches_plain_op() {
    let mut e = Engine::new();
    let got = rows(&mut e, "SELECT 2 OPERATOR(pg_catalog.+) 3");
    assert_eq!(as_i64(&got[0][0]), 5);
    // Unqualified form + comparison operator.
    let got = rows(&mut e, "SELECT 2 OPERATOR(<) 3");
    assert!(matches!(&got[0][0], spg_storage::Value::Bool(true)));
    // Precedence: multiplication binds tighter than addition even
    // through the explicit spelling.
    let got = rows(&mut e, "SELECT 2 OPERATOR(pg_catalog.+) 3 * 4");
    assert_eq!(as_i64(&got[0][0]), 14);
}
