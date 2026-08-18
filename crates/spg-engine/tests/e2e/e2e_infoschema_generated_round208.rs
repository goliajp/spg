//! v7.39 (read01 round 208) — information_schema.columns exposes
//! is_generated / generation_expression. Live-PG18.4 differential
//! (2026-07-18): a generated column (VIRTUAL or STORED) reports
//! is_generated='ALWAYS' + the parenthesized source expression; a
//! plain column reports 'NEVER' + NULL. Pre-r208 naming these columns
//! errored (they were absent), hiding generated columns from
//! reflection tools (SQLAlchemy / Alembic / pg_dump).

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        spg_storage::Value::Text(s) => s.to_string(),
                        v @ spg_storage::Value::SmallIntArray(_) => {
                            spg_engine::eval::value_to_text(v)
                        }
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn generated_columns_reported() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE gv (a INT, \
         b INT GENERATED ALWAYS AS (a*2) VIRTUAL, \
         c INT GENERATED ALWAYS AS (a+1) STORED)",
    )
    .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name, is_generated, generation_expression \
             FROM information_schema.columns WHERE table_name = 'gv' \
             ORDER BY column_name"
        ),
        vec![
            vec!["a".to_string(), "NEVER".to_string(), "NULL".to_string()],
            vec!["b".to_string(), "ALWAYS".to_string(), "(a * 2)".to_string()],
            vec!["c".to_string(), "ALWAYS".to_string(), "(a + 1)".to_string()],
        ]
    );
}
