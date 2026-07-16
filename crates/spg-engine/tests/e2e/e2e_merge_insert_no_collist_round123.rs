//! v7.39 (read01 round 123, Track A — nodeModifyTable.c 补读) — MERGE's
//! `WHEN NOT MATCHED THEN INSERT VALUES (…)` accepts an omitted column list.
//!
//! Read-driven scan of `src/backend/executor/nodeModifyTable.c` surfaced a
//! bounded parser gap: SPG required the `(cols)` list after INSERT in a MERGE
//! action, but PG (like a plain INSERT) makes it optional — an omitted list
//! fills every column in declaration order. Parser now allows it and the
//! executor maps the values positionally. Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE m (id int PRIMARY KEY, v int)").unwrap();
    e.execute("INSERT INTO m VALUES (1,10),(2,20)").unwrap();
    e.execute("CREATE TABLE src (id int, v int)").unwrap();
    e.execute("INSERT INTO src VALUES (2,88),(3,33),(4,44)").unwrap();
}

const AGG: &str = "SELECT string_agg(id||':'||v, ',' ORDER BY id) FROM m";

#[test]
fn merge_insert_without_column_list() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO m USING src ON m.id=src.id \
         WHEN MATCHED THEN UPDATE SET v=src.v \
         WHEN NOT MATCHED THEN INSERT VALUES(src.id,src.v)",
    )
    .unwrap();
    assert_eq!(text(&mut e, AGG), "1:10,2:88,3:33,4:44");
}

#[test]
fn merge_insert_with_explicit_column_list_still_works() {
    let mut e = Engine::new();
    setup(&mut e);
    e.execute(
        "MERGE INTO m USING src ON m.id=src.id \
         WHEN MATCHED THEN UPDATE SET v=src.v \
         WHEN NOT MATCHED THEN INSERT (id,v) VALUES(src.id,src.v)",
    )
    .unwrap();
    assert_eq!(text(&mut e, AGG), "1:10,2:88,3:33,4:44");
}

#[test]
fn merge_insert_too_many_values_errors() {
    let mut e = Engine::new();
    setup(&mut e);
    // No column list but more expressions than the table has columns.
    assert!(e
        .execute(
            "MERGE INTO m USING src ON m.id=src.id \
             WHEN NOT MATCHED THEN INSERT VALUES(src.id, src.v, 99)",
        )
        .is_err());
}
