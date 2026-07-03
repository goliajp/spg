//! TABLESAMPLE BERNOULLI / SYSTEM — row-level Bernoulli sampling
//! lowered to a random() < p/100 WHERE conjunct.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Int(n) => i64::from(*n),
        spg_storage::Value::BigInt(n) => *n,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn hundred_percent_keeps_all_zero_keeps_none() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ts (v INT)").unwrap();
    for i in 0..50 {
        e.execute(&format!("INSERT INTO ts VALUES ({i})")).unwrap();
    }
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM ts TABLESAMPLE BERNOULLI(100)"),
        50
    );
    assert_eq!(
        count(&mut e, "SELECT count(*) FROM ts TABLESAMPLE SYSTEM(0)"),
        0
    );
}

#[test]
fn fifty_percent_is_a_real_sample() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tb (v INT)").unwrap();
    let values: Vec<String> = (0..2000).map(|i| format!("({i})")).collect();
    e.execute(&format!("INSERT INTO tb VALUES {}", values.join(",")))
        .unwrap();
    // Binomial(2000, 0.5): stddev ~22; a 400-wide band on either
    // side is ~18 sigma — statistically impossible to flake.
    let n = count(&mut e, "SELECT count(*) FROM tb TABLESAMPLE BERNOULLI(50)");
    assert!((600..=1400).contains(&n), "50% of 2000 sampled {n}");
}

#[test]
fn combines_with_where_and_alias() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tw (v INT)").unwrap();
    e.execute("INSERT INTO tw VALUES (1), (2), (3), (4)").unwrap();
    // Sampling ANDs with the user WHERE; alias still qualifies.
    assert_eq!(
        count(
            &mut e,
            "SELECT count(*) FROM tw AS x TABLESAMPLE BERNOULLI(100) WHERE x.v > 2"
        ),
        2
    );
}

#[test]
fn repeatable_errors_honestly() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE tr (v INT)").unwrap();
    let err = e
        .execute("SELECT count(*) FROM tr TABLESAMPLE BERNOULLI(10) REPEATABLE(42)")
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(msg.contains("REPEATABLE"), "unexpected error: {msg}");
}
