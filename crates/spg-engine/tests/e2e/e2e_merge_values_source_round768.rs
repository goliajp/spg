//! Round 768 (F31-D5) — `MERGE INTO … USING (VALUES …) s(id, v)`.
//! PG executes this everyday form (the round-766 audit probe used it
//! and SPG answered a syntax error at VALUES). The source parses
//! through the same constant-SELECT lowering derived tables use, and
//! the positional column-alias list renames the materialised source
//! columns (PG's rule). Answers PG18-measured, byte-identical over
//! the wire (update + insert + delete arms).

use spg_engine::{Engine, QueryResult};

fn grid(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>()
            .join(";"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn round768_merge_values_source_answers_as_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE d5t (id INT, v TEXT)").unwrap();
    e.execute("INSERT INTO d5t VALUES (1, 'old'), (2, 'keep')").unwrap();
    e.execute(
        "MERGE INTO d5t t USING (VALUES (1, 'new'), (3, 'ins')) s(id, v) ON t.id = s.id \
         WHEN MATCHED THEN UPDATE SET v = s.v \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.v)",
    )
    .unwrap();
    assert_eq!(
        grid(&mut e, "SELECT * FROM d5t ORDER BY id"),
        "1|new;2|keep;3|ins"
    );
    e.execute(
        "MERGE INTO d5t t USING (VALUES (2, 'x')) s(id, v) ON t.id = s.id \
         WHEN MATCHED THEN DELETE",
    )
    .unwrap();
    assert_eq!(grid(&mut e, "SELECT * FROM d5t ORDER BY id"), "1|new;3|ins");
}
