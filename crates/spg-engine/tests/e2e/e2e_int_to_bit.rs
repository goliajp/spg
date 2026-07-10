//! v7.38 (read01, T20) — integer → bit(n) cast: the low n bits of the value's
//! two's-complement representation, right-aligned (int→varbit is rejected, as in
//! PG). Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows
            .into_iter()
            .next()
            .unwrap()
            .values
            .into_iter()
            .next()
            .unwrap(),
        _ => panic!("rows"),
    }
}

#[test]
fn int_to_bit() {
    let mut e = Engine::new();
    // Equality against the canonical B'...' literal is the tightest check.
    for (sql, lit) in [
        ("5::bit(4)", "B'0101'"),
        ("5::bit(8)", "B'00000101'"),
        ("5::bit(3)", "B'101'"),
        ("5::bit(1)", "B'1'"),
        ("5::bit", "B'1'"), // bare bit == bit(1)
        ("255::bit(4)", "B'1111'"),
        ("10::bit(4)", "B'1010'"),
        ("(-1)::bit(8)", "B'11111111'"),
    ] {
        let got = one(&mut e, &format!("SELECT ({sql} = {lit})"));
        assert!(
            matches!(got, spg_storage::Value::Bool(true)),
            "{sql} != {lit}"
        );
    }
    // Round-trips back to the integer.
    assert!(matches!(
        one(&mut e, "SELECT 5::bit(4)::int"),
        spg_storage::Value::Int(5)
    ));
}
