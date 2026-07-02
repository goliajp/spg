//! LATERAL generate_series — outer-column references as series
//! bounds (substitute_outer_in_table_ref channel, v7.37.43-T4.5).

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
fn outer_column_as_stop_bound() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE lo (id INT, n INT)").unwrap();
    e.execute("INSERT INTO lo VALUES (1, 2), (2, 3)").unwrap();
    // Each outer row fans out into n series rows: 2 + 3 = 5.
    let got = rows(
        &mut e,
        "SELECT lo.id, g FROM lo, LATERAL generate_series(1, lo.n) AS s(g) \
         ORDER BY lo.id, g",
    );
    assert_eq!(got.len(), 5);
    assert_eq!((as_i64(&got[1][0]), as_i64(&got[1][1])), (1, 2));
    assert_eq!((as_i64(&got[4][0]), as_i64(&got[4][1])), (2, 3));
}

#[test]
fn implicit_lateral_without_keyword() {
    let mut e = Engine::new();
    // PG allows the outer reference even without the LATERAL
    // keyword for SRFs in the FROM list.
    e.execute("CREATE TABLE li (n INT)").unwrap();
    e.execute("INSERT INTO li VALUES (2)").unwrap();
    let got = rows(
        &mut e,
        "SELECT g FROM li, generate_series(1, li.n) AS s(g) ORDER BY g",
    );
    assert_eq!(got.len(), 2);
    assert_eq!(as_i64(&got[1][0]), 2);
}
