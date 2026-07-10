//! TABLESAMPLE BERNOULLI / SYSTEM — row-level Bernoulli sampling
//! lowered to a random() < p/100 WHERE conjunct.

use spg_engine::{Engine, QueryResult};

fn count(e: &mut Engine, sql: &str) -> i64 {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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
    e.execute("INSERT INTO tw VALUES (1), (2), (3), (4)")
        .unwrap();
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
fn repeatable_is_deterministic() {
    // v7.38 (read01 U15) — REPEATABLE(seed) now yields a deterministic,
    // rescan-stable sample (was an honest "not supported" error before).
    // SPG's row order differs from PG's page order, so the exact rows
    // differ, but the observable contract — same seed → same sample on
    // repeat — matches PG.
    let mut e = Engine::new();
    e.execute("CREATE TABLE tr (v INT)").unwrap();
    for i in 0..1000 {
        e.execute(&format!("INSERT INTO tr VALUES({i})")).unwrap();
    }
    let c1 = count(
        &mut e,
        "SELECT count(*) FROM tr TABLESAMPLE BERNOULLI(30) REPEATABLE(42)",
    );
    let c2 = count(
        &mut e,
        "SELECT count(*) FROM tr TABLESAMPLE BERNOULLI(30) REPEATABLE(42)",
    );
    assert_eq!(c1, c2, "same seed must reproduce the same sample");
    // A different seed generally selects a different subset.
    let c3 = count(
        &mut e,
        "SELECT count(*) FROM tr TABLESAMPLE BERNOULLI(30) REPEATABLE(7)",
    );
    assert_ne!(c1, c3, "different seed should give a different sample");
    // Non-REPEATABLE still parses and samples.
    let _ = count(&mut e, "SELECT count(*) FROM tr TABLESAMPLE BERNOULLI(30)");
}
