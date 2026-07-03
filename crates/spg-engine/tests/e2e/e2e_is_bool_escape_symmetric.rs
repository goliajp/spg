//! IS [NOT] TRUE/FALSE/UNKNOWN + LIKE ESCAPE + BETWEEN SYMMETRIC.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn b(v: spg_storage::Value<'static>) -> bool {
    match v {
        spg_storage::Value::Bool(x) => x,
        other => panic!("expected bool, got {other:?}"),
    }
}

#[test]
fn is_true_false_unknown_three_valued() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE ib (v BOOLEAN)").unwrap();
    e.execute("INSERT INTO ib VALUES (true), (false), (NULL)")
        .unwrap();
    // NULL IS TRUE is false (not NULL) — the count proves the
    // three-valued lowering never yields NULL.
    let n = one(&mut e, "SELECT count(*) FROM ib WHERE v IS TRUE");
    assert!(matches!(n, spg_storage::Value::BigInt(1) | spg_storage::Value::Int(1)));
    let n = one(&mut e, "SELECT count(*) FROM ib WHERE v IS NOT TRUE");
    assert!(matches!(n, spg_storage::Value::BigInt(2) | spg_storage::Value::Int(2)));
    let n = one(&mut e, "SELECT count(*) FROM ib WHERE v IS FALSE");
    assert!(matches!(n, spg_storage::Value::BigInt(1) | spg_storage::Value::Int(1)));
    let n = one(&mut e, "SELECT count(*) FROM ib WHERE v IS UNKNOWN");
    assert!(matches!(n, spg_storage::Value::BigInt(1) | spg_storage::Value::Int(1)));
    assert!(b(one(&mut e, "SELECT (1 > 2) IS NOT TRUE")));
}

#[test]
fn like_escape_rewrites_pattern() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE le (t TEXT)").unwrap();
    e.execute("INSERT INTO le VALUES ('50% off'), ('50x off')")
        .unwrap();
    // '!%' escapes the wildcard — only the literal percent matches.
    let n = one(
        &mut e,
        "SELECT count(*) FROM le WHERE t LIKE '50!% off' ESCAPE '!'",
    );
    assert!(matches!(n, spg_storage::Value::BigInt(1) | spg_storage::Value::Int(1)));
    // Doubled escape char is the literal char.
    assert!(b(one(&mut e, "SELECT 'a!b' LIKE 'a!!b' ESCAPE '!'")));
}

#[test]
fn between_symmetric_swaps_bounds() {
    let mut e = Engine::new();
    assert!(b(one(&mut e, "SELECT 2 BETWEEN SYMMETRIC 3 AND 1")));
    assert!(!b(one(&mut e, "SELECT 2 BETWEEN 3 AND 1")));
    assert!(!b(one(&mut e, "SELECT 5 BETWEEN SYMMETRIC 3 AND 1")));
    // NOT + ASYMMETRIC noise word.
    assert!(b(one(&mut e, "SELECT 5 NOT BETWEEN ASYMMETRIC 1 AND 3")));
}
