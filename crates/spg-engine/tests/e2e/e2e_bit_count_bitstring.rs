//! v7.38 (read01) — bit_count(bit)/bit_count(varbit) returns the number of set
//! bits (PG had it for bit/varbit; SPG only accepted integer/bytea). Oracle:
//! live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn n(e: &mut Engine, sql: &str) -> i64 {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match rows[0].values[0] {
            spg_storage::Value::BigInt(v) => v,
            ref v => panic!("expected bigint, got {v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn bit_count_over_bit_strings() {
    let mut e = Engine::new();
    assert_eq!(n(&mut e, "SELECT bit_count(B'1010')"), 2);
    assert_eq!(n(&mut e, "SELECT bit_count(B'11111111')"), 8);
    assert_eq!(n(&mut e, "SELECT bit_count(B'1010101010101010')"), 8);
    assert_eq!(n(&mut e, "SELECT bit_count(B'101'::varbit)"), 2);
    // bytea form unchanged.
    assert_eq!(n(&mut e, "SELECT bit_count('\\xff0f'::bytea)"), 12);
}
