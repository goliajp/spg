//! Round 778 (F31-F1) — USING / NATURAL joins over VALUES lists and
//! derived tables, PG18-measured byte-identical: the schema resolver
//! reads the `x(id, v)` alias list (or the inner projection's own
//! output names), where SPG refused with "requires table sources
//! with known schemas".

use spg_engine::{Engine, QueryResult};

fn grid(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, columns } => {
            let mut out = vec![
                columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join("|"),
            ];
            out.extend(rows.iter().map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            }));
            out.join(";")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn round778_using_and_natural_over_values_answer_as_pg() {
    let mut e = Engine::new();
    assert_eq!(
        grid(
            &mut e,
            "SELECT * FROM (VALUES (1,'a')) x(id,v) JOIN (VALUES (1,'b')) y(id,w) USING (id)"
        ),
        "id|v|w;1|a|b"
    );
    assert_eq!(
        grid(
            &mut e,
            "SELECT * FROM (VALUES (1,'a'),(2,'c')) x(id,v) \
             LEFT JOIN (VALUES (1,'b')) y(id,w) USING (id) ORDER BY id"
        ),
        "id|v|w;1|a|b;2|c|NULL"
    );
    // NATURAL with no common columns cross-joins, as PG does.
    assert_eq!(
        grid(
            &mut e,
            "SELECT count(*) FROM (VALUES (1),(2)) a(x) NATURAL JOIN (VALUES (10),(20)) b(y)"
        ),
        "count;4"
    );
    assert_eq!(
        grid(
            &mut e,
            "SELECT * FROM (VALUES (1,'p')) a(k,s) NATURAL JOIN (VALUES (1,'q')) b(k,t)"
        ),
        "k|s|t;1|p|q"
    );
    // Derived-select source resolves through its projection names.
    assert_eq!(
        grid(
            &mut e,
            "SELECT id FROM (SELECT 1 AS id, 'z' AS z) d JOIN (VALUES (1,'b')) y(id,w) USING (id)"
        ),
        "id;1"
    );
}
