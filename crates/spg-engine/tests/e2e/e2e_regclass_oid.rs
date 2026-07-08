//! v7.38 (read01, T22) — a numeric OID cast to regclass reverse-looks-up the
//! user relation name (the `oid::regclass` introspection idiom). SPG assigns
//! user relations OIDs in the 16384+ band in catalog order; the exact values
//! are SPG-internal, so the test drives the round trip through pg_class rather
//! than hardcoding an OID.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn oid_to_regclass_reverse_lookup() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE reg_a(a int)").unwrap();
    e.execute("CREATE TABLE reg_b(b int)").unwrap();
    // The pg_class.oid → regclass idiom renders the relation name.
    assert_eq!(
        text(&mut e, "SELECT (oid::regclass)::text FROM pg_class WHERE relname='reg_a'"),
        "reg_a"
    );
    assert_eq!(
        text(&mut e, "SELECT (oid::regclass)::text FROM pg_class WHERE relname='reg_b'"),
        "reg_b"
    );
    // An OID with no matching relation renders as the integer (fallback).
    assert_eq!(text(&mut e, "SELECT (999999::regclass)::text"), "999999");
    // The text→regclass direction still strips the schema.
    assert_eq!(text(&mut e, "SELECT ('reg_a'::regclass)::text"), "reg_a");
}
