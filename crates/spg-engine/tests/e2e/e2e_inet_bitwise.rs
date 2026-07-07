//! v7.38 (read01) — PG inet bitwise operators `inet & inet`, `inet | inet`
//! and `~ inet`. Both operands of a binary op must share the family (else an
//! error, like PG); the result netmask is the wider of the two. Assertions use
//! inet equality to stay independent of the inet::text `/32` display quirk.
//! Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn b(e: &mut Engine, sql: &str) -> bool {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => matches!(rows[0].values[0], spg_storage::Value::Bool(true)),
        _ => panic!("expected rows"),
    }
}

#[test]
fn inet_bitwise_ops() {
    let mut e = Engine::new();
    assert!(b(&mut e, "SELECT ('192.168.1.5'::inet & '255.255.255.0'::inet) = '192.168.1.0'::inet"));
    assert!(b(&mut e, "SELECT ('10.0.0.0/8'::inet | '0.0.0.255/24'::inet) = '10.0.0.255/24'::inet"));
    assert!(b(&mut e, "SELECT (~ '255.0.0.0/8'::inet) = '0.255.255.255/8'::inet"));
    // IPv6 AND.
    assert!(b(&mut e, "SELECT ('2001:db8::ff'::inet & '::f0'::inet) = '::f0'::inet"));
    // Mixing families errors.
    assert!(e.execute("SELECT '10.1.2.3'::inet & '2001:db8::'::inet").is_err());
    // Integer bitwise is unaffected.
    assert!(b(&mut e, "SELECT (12 & 10) = 8"));
}
