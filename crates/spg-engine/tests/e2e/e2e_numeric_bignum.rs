//! v7.38 (read01, T3.C3) — NUMERIC arithmetic beyond i128 (~38 digits) promotes
//! to arbitrary precision instead of erroring, and demotes back when a result
//! fits i128 again. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn t(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn numeric_beyond_i128() {
    let mut e = Engine::new();
    // 38-digit × 20-digit = 58-digit product.
    assert_eq!(
        t(&mut e, "SELECT (12345678901234567890123456789012345678 * 98765432109876543210)::text"),
        "1219326311370217952249657064224965706333485749112223746380"
    );
    // 39-digit × 2.
    assert_eq!(
        t(&mut e, "SELECT (123456789012345678901234567890123456789::numeric * 2)::text"),
        "246913578024691357802469135780246913578"
    );
    // Chained big op (product is big, then + 1) via a NumericBig operand.
    assert_eq!(
        t(&mut e, "SELECT (99999999999999999999999999999999999999 * 99999999999999999999999999999999999999 + 1)::text"),
        "9999999999999999999999999999999999999800000000000000000000000000000000000002"
    );
    // A result that fits i128 again stays a normal Numeric.
    assert_eq!(t(&mut e, "SELECT (10000000000000000000 * 2)::text"), "20000000000000000000");
    // (Parsing a literal whose *mantissa* itself overflows i128 — a big literal
    // rather than a big *result* — is a follow-up C3 sub-item.)
}


#[test]
fn numeric_bignum_compare() {
    let mut e = Engine::new();
    let b = |e: &mut Engine, sql: &str| matches!(
        e.execute(sql).unwrap(),
        QueryResult::Rows { ref rows, .. } if matches!(rows[0].values[0], spg_storage::Value::Bool(true))
    );
    // Big vs big (produced via arithmetic), and big vs a small int.
    assert!(b(&mut e, "SELECT (99999999999999999999999999999999999999 * 2) > (99999999999999999999999999999999999999 * 1)"));
    assert!(b(&mut e, "SELECT (99999999999999999999999999999999999999 * 2) = (99999999999999999999999999999999999999 + 99999999999999999999999999999999999999)"));
    assert!(!b(&mut e, "SELECT (99999999999999999999999999999999999999 * 2) < 5"));
}
