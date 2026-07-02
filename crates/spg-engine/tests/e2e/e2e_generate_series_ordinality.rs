//! generate_series(...) WITH ORDINALITY + AS t(n) column aliasing.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.iter().map(|row| row.values.to_vec()).collect()
}

fn cols(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { columns, .. } = r else {
        panic!("expected Rows");
    };
    columns.iter().map(|c| c.name.clone()).collect()
}

fn as_i64(v: &spg_storage::Value<'_>) -> i64 {
    match v {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn ordinality_tracks_series_order() {
    let mut e = Engine::new();
    // Descending series: values 5,3,1 get ordinality 1,2,3.
    let got = rows(
        &mut e,
        "SELECT * FROM generate_series(5, 1, -2) WITH ORDINALITY",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(as_i64(&got[0][0]), 5);
    assert_eq!(as_i64(&got[0][1]), 1);
    assert_eq!(as_i64(&got[2][0]), 1);
    assert_eq!(as_i64(&got[2][1]), 3);
    // PG's default column names.
    assert_eq!(
        cols(&mut e, "SELECT * FROM generate_series(1, 2) WITH ORDINALITY"),
        ["generate_series", "ordinality"]
    );
}

#[test]
fn column_alias_renames_series_column() {
    let mut e = Engine::new();
    // AS t(n) — first entry renames the series column.
    let got = rows(&mut e, "SELECT n * 10 FROM generate_series(1, 3) AS t(n)");
    assert_eq!(got.len(), 3);
    assert_eq!(as_i64(&got[2][0]), 30);
}

#[test]
fn both_aliases_with_ordinality() {
    let mut e = Engine::new();
    let got = rows(
        &mut e,
        "SELECT i, v FROM generate_series(10, 12) WITH ORDINALITY AS t(v, i) \
         ORDER BY i DESC",
    );
    assert_eq!(got.len(), 3);
    assert_eq!(as_i64(&got[0][0]), 3);
    assert_eq!(as_i64(&got[0][1]), 12);
}
