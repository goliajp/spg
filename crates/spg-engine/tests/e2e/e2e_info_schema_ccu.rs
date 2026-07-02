//! v7.37.17 (17.6 siblings) —
//! information_schema.constraint_column_usage.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows.into_iter()
        .map(|row| row.values.into_iter().collect())
        .collect()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn pk_lists_own_columns_fk_lists_parent_columns() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE par (id INT PRIMARY KEY)").unwrap();
    e.execute(
        "CREATE TABLE chi (pid INT, \
         CONSTRAINT chi_fk FOREIGN KEY (pid) REFERENCES par(id))",
    )
    .unwrap();
    let got = rows(
        &mut e,
        "SELECT table_name, column_name, constraint_name \
         FROM information_schema.constraint_column_usage \
         ORDER BY constraint_name",
    );
    assert_eq!(got.len(), 2);
    // FK row references the PARENT's column (PG semantics).
    assert_eq!(text(&got[0][2]), "chi_fk");
    assert_eq!(text(&got[0][0]), "par");
    assert_eq!(text(&got[0][1]), "id");
    // PK row lists its own column.
    assert_eq!(text(&got[1][2]), "par_pkey");
    assert_eq!(text(&got[1][0]), "par");
    assert_eq!(text(&got[1][1]), "id");
}
