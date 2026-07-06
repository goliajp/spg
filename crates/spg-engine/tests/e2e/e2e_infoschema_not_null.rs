//! v7.38 (read01 P6.37) — PG 18 records each NOT NULL column as a CHECK
//! constraint (`{table}_{col}_not_null`) in
//! information_schema.table_constraints. Oracle values from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<spg_storage::Value<'static>>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows.iter().map(|r| r.values.clone()).collect(),
        _ => panic!("expected rows"),
    }
}

#[test]
fn not_null_columns_appear_as_check_constraints() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tcnn(id int PRIMARY KEY, x int NOT NULL, y int UNIQUE)")
        .unwrap();
    // constraint_type histogram: 2 CHECK (id + x NOT NULL), 1 PK, 1 UNIQUE.
    let hist = rows(
        &mut e,
        "SELECT constraint_type, count(*) FROM information_schema.table_constraints \
         WHERE table_name='tcnn' GROUP BY constraint_type ORDER BY 1",
    );
    assert_eq!(hist[0][0], spg_storage::Value::text("CHECK"));
    assert_eq!(hist[0][1], spg_storage::Value::BigInt(2));
    assert_eq!(hist[1][0], spg_storage::Value::text("PRIMARY KEY"));
    assert_eq!(hist[2][0], spg_storage::Value::text("UNIQUE"));

    // The NOT NULL constraint names follow PG's {table}_{col}_not_null.
    let names = rows(
        &mut e,
        "SELECT constraint_name FROM information_schema.table_constraints \
         WHERE table_name='tcnn' AND constraint_type='CHECK' ORDER BY constraint_name",
    );
    assert_eq!(names[0][0], spg_storage::Value::text("tcnn_id_not_null"));
    assert_eq!(names[1][0], spg_storage::Value::text("tcnn_x_not_null"));
}
