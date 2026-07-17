//! v7.39 (read01 round 135 — GROUPING() in ORDER BY) — PG allows `grouping()`
//! (and expressions over it) in the ORDER BY of a grouping-set query
//! (ROLLUP / CUBE / GROUPING SETS). SPG desugars grouping sets to UNION ALL in
//! the parser, so `grouping()` in the outer ORDER BY errored with
//! "unknown function grouping". Now the parser injects a per-branch hidden
//! `grouping(...) AS __grp_ord_K` column, rewrites the ORDER BY to reference it,
//! and the engine strips the `__grp_ord_` columns from the final output.
//! Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn rows(e: &mut Engine, sql: &str) -> Vec<Vec<String>> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => "NULL".to_string(),
                        v => spg_engine::eval::value_to_text(v),
                    })
                    .collect()
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn setup(e: &mut Engine) {
    e.execute("CREATE TABLE gt(a int, b int, v int)").unwrap();
    e.execute("INSERT INTO gt VALUES(1,10,100),(1,20,200),(2,10,50),(2,20,80)")
        .unwrap();
}

#[test]
fn c1_grouping_in_select_and_order_by() {
    let mut e = Engine::new();
    setup(&mut e);
    // grouping(a)/grouping(b) in both SELECT and ORDER BY.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a,b,sum(v),grouping(a) ga,grouping(b) gb FROM gt \
             GROUP BY ROLLUP(a,b) ORDER BY grouping(a),grouping(b),a,b"
        ),
        vec![
            vec!["1", "10", "100", "0", "0"],
            vec!["1", "20", "200", "0", "0"],
            vec!["2", "10", "50", "0", "0"],
            vec!["2", "20", "80", "0", "0"],
            vec!["1", "NULL", "300", "0", "1"],
            vec!["2", "NULL", "130", "0", "1"],
            vec!["NULL", "NULL", "430", "1", "1"],
        ]
    );
}

#[test]
fn c2_grouping_only_in_order_by() {
    let mut e = Engine::new();
    setup(&mut e);
    // grouping(a) NOT in the select list — only in ORDER BY, DESC.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a,sum(v) FROM gt GROUP BY ROLLUP(a) ORDER BY grouping(a) DESC, a"
        ),
        vec![vec!["NULL", "430"], vec!["1", "300"], vec!["2", "130"],]
    );
}

#[test]
fn c3_grouping_expression_in_order_by() {
    let mut e = Engine::new();
    setup(&mut e);
    // grouping(a)+grouping(b) expression in ORDER BY, over CUBE.
    assert_eq!(
        rows(
            &mut e,
            "SELECT a,b,sum(v) FROM gt GROUP BY CUBE(a,b) \
             ORDER BY grouping(a)+grouping(b), a NULLS FIRST, b NULLS FIRST"
        ),
        vec![
            vec!["1", "10", "100"],
            vec!["1", "20", "200"],
            vec!["2", "10", "50"],
            vec!["2", "20", "80"],
            vec!["NULL", "10", "150"],
            vec!["NULL", "20", "280"],
            vec!["1", "NULL", "300"],
            vec!["2", "NULL", "130"],
            vec!["NULL", "NULL", "430"],
        ]
    );
}

#[test]
fn c4_grouping_sets_explicit() {
    let mut e = Engine::new();
    setup(&mut e);
    assert_eq!(
        rows(
            &mut e,
            "SELECT a,b,sum(v) FROM gt GROUP BY GROUPING SETS ((a,b),(a),()) \
             ORDER BY grouping(a),grouping(b),a,b"
        ),
        vec![
            vec!["1", "10", "100"],
            vec!["1", "20", "200"],
            vec!["2", "10", "50"],
            vec!["2", "20", "80"],
            vec!["1", "NULL", "300"],
            vec!["2", "NULL", "130"],
            vec!["NULL", "NULL", "430"],
        ]
    );
}
