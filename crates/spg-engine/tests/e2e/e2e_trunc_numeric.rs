//! v7.38 (read01) — trunc(numeric, n) returns NUMERIC (there is no
//! double-precision 2-arg trunc in PG), keeping the exact decimal rather than
//! routing through f64 and losing precision. Oracle: live PG 18.4.

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
fn trunc_numeric_stays_numeric() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT trunc(3.14159, 2)::text"), "3.14");
    // Exact past f64 precision (float would drift).
    assert_eq!(
        text(&mut e, "SELECT trunc(1234567.891234567891, 8)::text"),
        "1234567.89123456"
    );
    // Truncation is toward zero.
    assert_eq!(text(&mut e, "SELECT trunc(-3.14159, 2)::text"), "-3.14");
    // Negative target scale.
    assert_eq!(text(&mut e, "SELECT trunc(12345.0, -2)::text"), "12300");
    // Scaling up pads zeros.
    assert_eq!(text(&mut e, "SELECT trunc(3.1, 4)::text"), "3.1000");
}
