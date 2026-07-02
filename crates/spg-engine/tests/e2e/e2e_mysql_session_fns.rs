//! v7.37.17 (17.6 siblings) — MySQL session/utility functions:
//! rand / connection_id / sleep / benchmark / found_rows /
//! last_insert_id / row_count / uuid_short / is_uuid.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn rand_in_unit_interval() {
    let mut e = Engine::new();
    for _ in 0..8 {
        match first(&mut e, "SELECT rand()") {
            spg_storage::Value::Float(f) => {
                assert!((0.0..1.0).contains(&f), "rand() = {f}");
            }
            other => panic!("got {other:?}"),
        }
    }
    // Seeded form also lands in [0, 1).
    match first(&mut e, "SELECT rand(42)") {
        spg_storage::Value::Float(f) => assert!((0.0..1.0).contains(&f)),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn session_probes() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT connection_id()"),
        spg_storage::Value::BigInt(1)
    ));
    assert!(matches!(
        first(&mut e, "SELECT sleep(0)"),
        spg_storage::Value::Int(0)
    ));
    assert!(matches!(
        first(&mut e, "SELECT benchmark(1000, 1)"),
        spg_storage::Value::Int(0)
    ));
    assert!(matches!(
        first(&mut e, "SELECT found_rows()"),
        spg_storage::Value::BigInt(0)
    ));
    assert!(matches!(
        first(&mut e, "SELECT last_insert_id()"),
        spg_storage::Value::BigInt(0)
    ));
    assert!(matches!(
        first(&mut e, "SELECT row_count()"),
        spg_storage::Value::BigInt(-1)
    ));
}

#[test]
fn uuid_short_nonnegative_and_distinct() {
    let mut e = Engine::new();
    let a = match first(&mut e, "SELECT uuid_short()") {
        spg_storage::Value::BigInt(v) => v,
        other => panic!("got {other:?}"),
    };
    let b = match first(&mut e, "SELECT uuid_short()") {
        spg_storage::Value::BigInt(v) => v,
        other => panic!("got {other:?}"),
    };
    assert!(a >= 0 && b >= 0);
    assert_ne!(a, b);
}

#[test]
fn is_uuid_validates_forms() {
    let mut e = Engine::new();
    // Dashed 36-char form.
    assert!(matches!(
        first(
            &mut e,
            "SELECT is_uuid('6ccd780c-baba-1026-9564-5b8c656024db')"
        ),
        spg_storage::Value::Bool(true)
    ));
    // Bare 32-char hex form.
    assert!(matches!(
        first(&mut e, "SELECT is_uuid('6ccd780cbaba102695645b8c656024db')"),
        spg_storage::Value::Bool(true)
    ));
    // Invalid.
    assert!(matches!(
        first(&mut e, "SELECT is_uuid('not-a-uuid')"),
        spg_storage::Value::Bool(false)
    ));
    assert!(matches!(
        first(&mut e, "SELECT is_uuid(NULL::text)"),
        spg_storage::Value::Null
    ));
}
