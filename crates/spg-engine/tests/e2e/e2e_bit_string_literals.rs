//! B'1010' / X'1F' bit-string literals — lowered onto ::bit.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

#[test]
fn binary_form() {
    let mut e = Engine::new();
    let v = one(&mut e, "SELECT B'1010'");
    let spg_storage::Value::BitString { nbits, bytes } = v else {
        panic!("expected BitString, got {v:?}");
    };
    assert_eq!(nbits, 4);
    assert_eq!(bytes[0] >> 4, 0b1010);
    // Bad digit errors.
    assert!(e.execute("SELECT B'102'").is_err());
}

#[test]
fn hex_form_expands_to_bits() {
    let mut e = Engine::new();
    // X'1F' = 0001 1111 — 8 bits.
    let v = one(&mut e, "SELECT X'1F'");
    let spg_storage::Value::BitString { nbits, bytes } = v else {
        panic!("expected BitString, got {v:?}");
    };
    assert_eq!(nbits, 8);
    assert_eq!(bytes[0], 0x1F);
    // Case-insensitive prefix + digits.
    let v = one(&mut e, "SELECT x'a'");
    let spg_storage::Value::BitString { nbits, .. } = v else {
        panic!("expected BitString, got {v:?}");
    };
    assert_eq!(nbits, 4);
    assert!(e.execute("SELECT X'G1'").is_err());
}
