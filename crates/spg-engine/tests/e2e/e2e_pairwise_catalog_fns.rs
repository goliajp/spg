//! v7.37.17 (17.6 siblings) — pairwise catalog internals:
//! *_larger / *_smaller + textcat / byteacat.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn larger_smaller_pairs() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT int4larger(3, 7)"),
        spg_storage::Value::Int(7)
    ));
    assert!(matches!(
        first(&mut e, "SELECT int4smaller(3, 7)"),
        spg_storage::Value::Int(3)
    ));
    assert!(matches!(
        first(&mut e, "SELECT int8larger(300000000000, 7)"),
        spg_storage::Value::BigInt(300_000_000_000)
    ));
    match first(&mut e, "SELECT text_larger('apple', 'banana')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "banana"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT text_smaller('apple', 'banana')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "apple"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT float8larger(1.5, 2.5)") {
        spg_storage::Value::Float(f) => assert!((f - 2.5).abs() < 1e-12),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn textcat_strict_concat() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT textcat('foo', 'bar')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "foobar"),
        other => panic!("got {other:?}"),
    }
    // Strict: NULL in → NULL out (unlike variadic concat()).
    assert!(matches!(
        first(&mut e, "SELECT textcat('foo', NULL::text)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn byteacat_concatenates_bytes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT byteacat('ab'::bytea, 'cd'::bytea)") {
        spg_storage::Value::Bytes(b) => assert_eq!(b.as_ref(), b"abcd"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn pairwise_null_passthrough() {
    let mut e = Engine::new();
    // *_larger with one NULL returns the other value (greatest
    // semantics — PG's MAX aggregate internals skip NULLs).
    assert!(matches!(
        first(&mut e, "SELECT int4larger(NULL::int, 5)"),
        spg_storage::Value::Int(5)
    ));
    assert!(matches!(
        first(&mut e, "SELECT int4larger(NULL::int, NULL::int)"),
        spg_storage::Value::Null
    ));
}
