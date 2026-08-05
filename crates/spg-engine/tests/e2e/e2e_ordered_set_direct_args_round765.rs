//! Round 765 (F31-D2) — an ordered-set aggregate's DIRECT arguments
//! must use only grouped columns, PG18-measured (round-764 audit):
//! `percentile_cont(x) WITHIN GROUP (ORDER BY x)` refuses with PG's
//! "must appear in the GROUP BY clause" sentence, while a grouped
//! reference and constant shapes (including ARRAY fractions) answer.
//! SPG evaluated the first row's value and answered. Root-cause
//! sibling: the column visitor had no Expr::Array arm, so an ARRAY
//! constructor read as an unattributable BAIL column.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
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
fn round765_ungrouped_direct_arg_refuses_grouped_and_constant_answer() {
    let mut e = Engine::new();
    let err = format!(
        "{}",
        e.execute(
            "SELECT percentile_cont(x::float8) WITHIN GROUP (ORDER BY x) \
             FROM (VALUES (0.5)) t(x)"
        )
        .expect_err("ungrouped direct arg must refuse")
    );
    assert!(err.contains("must appear in the GROUP BY clause"), "{err}");
    // A grouped reference is legal.
    assert_eq!(
        one(
            &mut e,
            "SELECT g, percentile_cont(g::float8/10) WITHIN GROUP (ORDER BY x) \
             FROM (VALUES (5,1.0),(5,2.0)) t(g,x) GROUP BY g"
        ),
        "5|1.5"
    );
    // Constant shapes keep answering — including the ARRAY fraction
    // form that tripped the visitor's BAIL marker.
    assert_eq!(
        one(
            &mut e,
            "SELECT percentile_cont(ARRAY[0.25,0.5,0.75]) WITHIN GROUP (ORDER BY x) \
             FROM (VALUES (1.0),(2.0),(3.0),(4.0)) t(x)"
        ),
        "{1.75,2.5,3.25}"
    );
}
