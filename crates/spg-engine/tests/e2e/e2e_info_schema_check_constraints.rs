//! v7.37.17 (17.6 siblings) — information_schema.check_constraints.

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
fn check_constraints_listed_with_clause() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE cc (age INT CHECK (age >= 0), name TEXT)")
        .unwrap();
    let got = rows(
        &mut e,
        "SELECT constraint_name, check_clause \
         FROM information_schema.check_constraints",
    );
    assert_eq!(got.len(), 1);
    assert_eq!(text(&got[0][0]), "cc_check0");
    let clause = text(&got[0][1]);
    assert!(
        clause.contains("age") && clause.contains(">="),
        "clause: {clause}"
    );
}

#[test]
fn check_constraints_empty_without_checks() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nc (v INT)").unwrap();
    let got = rows(
        &mut e,
        "SELECT * FROM information_schema.check_constraints",
    );
    assert!(got.is_empty());
}
