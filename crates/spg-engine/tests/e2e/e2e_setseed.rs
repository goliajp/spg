//! v7.37.17 (17.6 siblings) — setseed(f) reseeds the PRNG so
//! random()/random_normal()/gen_random_uuid() become reproducible.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn setseed_returns_void() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT setseed(0.5)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn setseed_makes_random_reproducible() {
    let mut e = Engine::new();
    // Set seed A, take 3 random values.
    let _ = e.execute("SELECT setseed(0.42)").unwrap();
    let mut seq_a: Vec<f64> = Vec::new();
    for _ in 0..3 {
        match first(&mut e, "SELECT random()") {
            spg_storage::Value::Float(f) => seq_a.push(f),
            other => panic!("got {other:?}"),
        }
    }
    // Re-set the same seed, sequence should repeat exactly.
    let _ = e.execute("SELECT setseed(0.42)").unwrap();
    for expected in &seq_a {
        match first(&mut e, "SELECT random()") {
            spg_storage::Value::Float(f) => assert_eq!(
                f.to_bits(),
                expected.to_bits(),
                "reseeded random() must reproduce sequence"
            ),
            other => panic!("got {other:?}"),
        }
    }
}

#[test]
fn setseed_out_of_range_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT setseed(2.0)").is_err());
    assert!(e.execute("SELECT setseed(-2.0)").is_err());
}

#[test]
fn setseed_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT setseed(NULL::float)"),
        spg_storage::Value::Null
    ));
}
