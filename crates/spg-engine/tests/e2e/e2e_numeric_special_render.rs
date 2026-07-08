//! v7.38 (read01, T6.P2) — PG NUMERIC specials parse and render: 'NaN',
//! 'Infinity', '-Infinity' (case-insensitive, 'inf'/'-inf' accepted) round-trip
//! through ::numeric to their PG spellings. Arithmetic / comparison propagation
//! is a later phase (P3). Oracle: live PG 18.4.

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
fn numeric_specials_parse_and_render() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT ('NaN'::numeric)::text"), "NaN");
    assert_eq!(text(&mut e, "SELECT ('nan'::numeric)::text"), "NaN");
    assert_eq!(text(&mut e, "SELECT ('Infinity'::numeric)::text"), "Infinity");
    assert_eq!(text(&mut e, "SELECT ('inf'::numeric)::text"), "Infinity");
    assert_eq!(text(&mut e, "SELECT ('+Infinity'::numeric)::text"), "Infinity");
    assert_eq!(text(&mut e, "SELECT ('-Infinity'::numeric)::text"), "-Infinity");
    assert_eq!(text(&mut e, "SELECT ('-inf'::numeric)::text"), "-Infinity");
    // Concatenation renders the special spelling, not a Debug/zero.
    assert_eq!(text(&mut e, "SELECT 'x=' || 'NaN'::numeric"), "x=NaN");
    // Finite values are unaffected.
    assert_eq!(text(&mut e, "SELECT (3.14::numeric)::text"), "3.14");
}
