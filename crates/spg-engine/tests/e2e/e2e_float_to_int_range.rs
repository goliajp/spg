//! v7.38 (read01) — casting a FLOAT8 to int/bigint errors on a non-finite or
//! out-of-range value ('inf'::int, 1e20::int, 'nan'::int) instead of
//! saturating to i32::MAX / 0. Oracle: live PG 18.4.

use spg_engine::Engine;

#[test]
fn float_to_int_rejects_nonfinite_and_overflow() {
    let mut e = Engine::new();
    // Non-finite and out-of-range → error.
    assert!(e.execute("SELECT 'inf'::float8::int").is_err());
    assert!(e.execute("SELECT '-inf'::float8::int").is_err());
    assert!(e.execute("SELECT 'nan'::float8::int").is_err());
    assert!(e.execute("SELECT 1e20::float8::int").is_err());
    assert!(e.execute("SELECT 'inf'::float8::bigint").is_err());
    assert!(e.execute("SELECT 1e30::float8::bigint").is_err());
    // In-range values still cast (half-to-even rounding).
    use spg_engine::QueryResult;
    let cell = |e: &mut Engine, sql: &str| -> String {
        match e.execute(sql).unwrap() {
            QueryResult::Rows { rows, .. } => format!("{:?}", rows[0].values[0]),
            _ => panic!(),
        }
    };
    assert_eq!(cell(&mut e, "SELECT 3.7::float8::int"), "Int(4)");
    assert_eq!(
        cell(&mut e, "SELECT (-42.3)::float8::bigint"),
        "BigInt(-42)"
    );
    assert_eq!(
        cell(&mut e, "SELECT 2147483647.0::float8::int"),
        "Int(2147483647)"
    );
}
