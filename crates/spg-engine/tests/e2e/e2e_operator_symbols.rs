//! PG operator symbols: regex match ~ / ~* / !~ / !~*, ^@
//! starts-with, ^ power, # integer XOR, << / >> integer shifts.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match one(e, sql) {
        spg_storage::Value::Bool(v) => v,
        other => panic!("{sql}: expected Bool, got {other:?}"),
    }
}

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn regex_match_operators() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT 'abc' ~ 'b'"));
    assert!(!b(&mut e, "SELECT 'abc' ~ 'z'"));
    // Case-insensitive variant.
    assert!(b(&mut e, "SELECT 'ABC' ~* 'b'"));
    assert!(!b(&mut e, "SELECT 'abc' ~ 'B'"));
    // Negated forms.
    assert!(b(&mut e, "SELECT 'abc' !~ 'z'"));
    assert!(!b(&mut e, "SELECT 'abc' !~ 'b'"));
    assert!(!b(&mut e, "SELECT 'ABC' !~* 'b'"));
}

#[test]
fn starts_with_operator() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT 'abcdef' ^@ 'abc'"));
    assert!(!b(&mut e, "SELECT 'abcdef' ^@ 'xyz'"));
}

#[test]
fn power_and_xor() {
    let mut e = Engine::new();
    // ^ binds tighter than +.
    assert!(matches!(one(&mut e, "SELECT 2 ^ 10"), spg_storage::Value::Float(f) if (f - 1024.0).abs() < 1e-9));
    assert!(matches!(one(&mut e, "SELECT 3 + 2 ^ 2"), spg_storage::Value::Float(f) if (f - 7.0).abs() < 1e-9));
    // # integer XOR.
    let xor = |e: &mut Engine, sql: &str| match one(e, sql) {
        spg_storage::Value::Int(n) => i64::from(n),
        spg_storage::Value::BigInt(n) => n,
        other => panic!("expected int, got {other:?}"),
    };
    assert_eq!(xor(&mut e, "SELECT 7 # 3"), 4);
    assert_eq!(xor(&mut e, "SELECT 5 # 5"), 0);
    assert_eq!(xor(&mut e, "SELECT 255 # 0"), 255);
}

#[test]
fn integer_shifts() {
    let mut e = Engine::new();
    let sh = |e: &mut Engine, sql: &str| match one(e, sql) {
        spg_storage::Value::Int(n) => i64::from(n),
        spg_storage::Value::BigInt(n) => n,
        other => panic!("expected int, got {other:?}"),
    };
    assert_eq!(sh(&mut e, "SELECT 1 << 4"), 16);
    assert_eq!(sh(&mut e, "SELECT 16 >> 2"), 4);
}
