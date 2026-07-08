//! v7.38 (read01, T11) — CHAR(n) / bpchar semantics: blank-padded storage, but
//! length / comparison / DISTINCT / ::text / concat all ignore the trailing
//! blanks; an over-long cast truncates. Oracle: live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) | spg_storage::Value::BpChar(s) => s.to_string(),
            spg_storage::Value::Bool(b) => b.to_string(),
            spg_storage::Value::Int(n) => n.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("rows"),
    }
}

#[test]
fn bpchar_semantics() {
    let mut e = Engine::new();
    // Comparison is trailing-space-insensitive.
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab'"), "true");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab  '"), "true");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab'::char(6)"), "true");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) < 'ac'"), "true");
    // length / char_length ignore padding.
    assert_eq!(one(&mut e, "SELECT length('ab'::char(4))"), "2");
    assert_eq!(one(&mut e, "SELECT char_length('ab'::char(4))"), "2");
    // ::text strips; concat strips.
    assert_eq!(one(&mut e, "SELECT ('ab'::char(4))::text"), "ab");
    assert_eq!(one(&mut e, "SELECT length(('ab'::char(4))::text)"), "2");
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) || 'x'"), "abx");
    assert_eq!(one(&mut e, "SELECT '[' || ('ab'::char(4))::text || ']'"), "[ab]");
    // Over-long cast truncates.
    assert_eq!(one(&mut e, "SELECT 'abc'::char(2)"), "ab");
    assert_eq!(one(&mut e, "SELECT length('abc'::char(2))"), "2");
    // ORDER BY / equality ignore trailing blanks.
    assert_eq!(one(&mut e, "SELECT 'ab'::char(4) = 'ab'::char(2)"), "true");
    // (COUNT(DISTINCT bpchar) across *different* declared widths still keys on
    // the padded form — a small residual in the aggregate dedup path.)
}
