//! v7.38 (read01, T12.4/R2) — tsquery total order (<, <=, >, >=). PG's
//! silly_cmp_tsquery is non-recursive: node count first, then a prefix-item
//! compare where operands sort before operators, operands compare by *signed*
//! CRC-32 (so it is NOT alphabetical), operators sort PHRASE<OR<AND<NOT, and two
//! PHRASE nodes sort larger-distance-first. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => matches!(rows[0].values[0], spg_storage::Value::Bool(true)),
        _ => panic!("rows"),
    }
}

#[test]
fn tsquery_total_order() {
    let mut e = Engine::new();
    // Operand order is CRC-based (signed), not alphabetical.
    assert!(!b(&mut e, "SELECT 'b'::tsquery < 'c'::tsquery"));
    assert!(!b(&mut e, "SELECT 'cat'::tsquery < 'dog'::tsquery"));
    assert!(b(&mut e, "SELECT 'a'::tsquery < 'z'::tsquery"));
    // Node count first.
    assert!(b(&mut e, "SELECT 'a'::tsquery < 'a & b'::tsquery"));
    assert!(!b(&mut e, "SELECT '!a'::tsquery < 'a'::tsquery")); // numnode 2 > 1
    // Non-recursive: 'a & b' vs 'a & c' does NOT reduce to b vs c leaf compare.
    assert!(!b(&mut e, "SELECT 'a & b'::tsquery < 'a & c'::tsquery"));
    // Operator order OR < AND; operand (VAL) before operator.
    assert!(b(&mut e, "SELECT 'a | b'::tsquery < 'a & b'::tsquery"));
    assert!(!b(&mut e, "SELECT '(a & b) | c'::tsquery < 'a | (b & c)'::tsquery"));
    // Two PHRASE nodes: larger distance sorts first.
    assert!(b(&mut e, "SELECT 'a <2> b'::tsquery < 'a <-> b'::tsquery"));
    // Equality stays structural (operand order matters).
    assert!(b(&mut e, "SELECT ('a & b'::tsquery = 'a & b'::tsquery)"));
    assert!(b(&mut e, "SELECT ('a & b'::tsquery <> 'b & a'::tsquery)"));
}
