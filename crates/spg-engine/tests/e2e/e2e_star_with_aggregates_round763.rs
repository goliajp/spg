//! Round 763 (F31-C1) — `SELECT *, count(*) … GROUP BY <all columns>`
//! is legal PG (the wildcard expands to grouped columns; the
//! star_and_aggregate_with_group_by pin even called it a "known gap").
//! SPG refused the whole shape with "SELECT * with aggregates is not
//! supported". The wildcard now expands to explicit column refs up
//! front (single plain-table FROM), and the existing validation
//! answers PG's exact sentence for any non-grouped column.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, columns } => {
            let mut out = vec![columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("|")];
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
fn round763_star_with_aggregates_answers_as_pg() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE c1t (id INT, name TEXT)").unwrap();
    e.execute("INSERT INTO c1t VALUES (1,'x'),(1,'x'),(2,'y')").unwrap();
    for sql in [
        "SELECT *, count(*) FROM c1t GROUP BY id, name ORDER BY id",
        "SELECT c1t.*, count(*) FROM c1t GROUP BY id, name ORDER BY id",
    ] {
        assert_eq!(one(&mut e, sql), "id|name|count;1|x|2;2|y|1", "{sql}");
    }
    assert_eq!(
        one(&mut e, "SELECT t.*, sum(id) FROM c1t t GROUP BY id, name ORDER BY id"),
        "id|name|sum;1|x|2;2|y|2"
    );
    // A non-grouped column refuses with PG's sentence.
    let err = format!(
        "{}",
        e.execute("SELECT *, count(*) FROM c1t GROUP BY id")
            .expect_err("ungrouped name must refuse")
    );
    assert!(err.contains("must appear in the GROUP BY clause"), "{err}");
    let err = format!(
        "{}",
        e.execute("SELECT *, count(*) FROM c1t")
            .expect_err("no GROUP BY at all must refuse")
    );
    assert!(err.contains("must appear in the GROUP BY clause"), "{err}");
}
