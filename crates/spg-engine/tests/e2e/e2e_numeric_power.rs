//! v7.38 (read01, T5) — a NUMERIC base raised to a non-negative integer
//! exponent returns NUMERIC (PG types numeric ^ as numeric), with PG's display
//! scale (rscale = 17 − integer-digit count, or 16 for |result| < 1). An
//! integer base stays double; fractional exponents remain double for now.
//! Oracle: live PG 18.4.

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
fn numeric_integer_power() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT (2.0^10)::text"), "1024.0000000000000");
    assert_eq!(text(&mut e, "SELECT (power(2.0,3))::text"), "8.0000000000000000");
    assert_eq!(text(&mut e, "SELECT (10.0^5)::text"), "100000.00000000000");
    assert_eq!(text(&mut e, "SELECT (3.0^4)::text"), "81.000000000000000");
    assert_eq!(text(&mut e, "SELECT (2.5^2)::text"), "6.2500000000000000");
    assert_eq!(text(&mut e, "SELECT (100.0^2)::text"), "10000.000000000000");
    assert_eq!(text(&mut e, "SELECT (2.0^20)::text"), "1048576.0000000000");
    assert_eq!(text(&mut e, "SELECT (0.5^3)::text"), "0.1250000000000000");
    assert_eq!(text(&mut e, "SELECT (2.0^0)::text"), "1.0000000000000000");
    assert_eq!(text(&mut e, "SELECT (power(123.0,2))::text"), "15129.000000000000");
    assert_eq!(text(&mut e, "SELECT pg_typeof(2.0^10)"), "numeric");
    // Integer base stays double precision (matching PG).
    assert_eq!(text(&mut e, "SELECT (2^10)::text"), "1024");
    assert_eq!(text(&mut e, "SELECT pg_typeof(2^10)"), "double precision");
}
