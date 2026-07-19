//! v7.37.17 (17.6 siblings) —
//! information_schema.constraint_column_usage.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    // Live PG 18.4 on this exact fixture:
    //   par|id|chi_fk;par|id|par_id_not_null;par|id|par_pkey
    // The middle row is the NOT NULL the PRIMARY KEY implies. This pin
    // asserted two rows until round 266, which locked in its absence.
    let got: Vec<String> = got
        .iter()
        .map(|r| alloc_join(&[text(&r[0]), text(&r[1]), text(&r[2])]))
        .collect();
    assert_eq!(
        got,
        vec!["par|id|chi_fk", "par|id|par_id_not_null", "par|id|par_pkey"],
    );
}

fn alloc_join(parts: &[String]) -> String {
    parts.join("|")
}
