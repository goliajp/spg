//! v7.38 (read01) — int2 (smallint) arithmetic keeps the int2 type, matching
//! PG: `int2 <op> int2` is smallint (widening only when mixed with int4/int8),
//! and a result outside int2 is "smallint out of range", not a silent widen.
//! Covers +/-/*///%, & | # (bitwise), << (shift) and ~ (bitnot). Every
//! expected value / type / error is from live PG18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    match e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            other => panic!("{sql}: expected Text, got {other:?}"),
        },
        other => panic!("{sql}: expected Rows, got {other:?}"),
    }
}

#[test]
fn int2_arithmetic_stays_int2() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 + 3::int2)::text"),
        "smallint"
    );
    assert_eq!(one(&mut e, "SELECT (5::int2 + 3::int2)::text"), "8");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 - 9::int2)::text"),
        "smallint"
    );
    assert_eq!(one(&mut e, "SELECT (5::int2 - 9::int2)::text"), "-4");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 * 4::int2)::text"),
        "smallint"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(7::int2 / 2::int2)::text"),
        "smallint"
    );
    assert_eq!(one(&mut e, "SELECT (7::int2 / 2::int2)::text"), "3");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(7::int2 % 3::int2)::text"),
        "smallint"
    );
}

#[test]
fn int2_bitwise_and_shift_stay_int2() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 & 3::int2)::text"),
        "smallint"
    );
    assert_eq!(one(&mut e, "SELECT (5::int2 & 3::int2)::text"), "1");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 | 2::int2)::text"),
        "smallint"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 # 1::int2)::text"),
        "smallint"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(~(5::int2))::text"),
        "smallint"
    );
    assert_eq!(one(&mut e, "SELECT (~(5::int2))::text"), "-6");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 << 1)::text"),
        "smallint"
    );
    assert_eq!(one(&mut e, "SELECT (5::int2 << 2)::text"), "20");
}

#[test]
fn int2_widens_only_when_mixed() {
    let mut e = Engine::new();
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 + 3::int4)::text"),
        "integer"
    );
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(5::int2 + 3::int8)::text"),
        "bigint"
    );
}

#[test]
fn int2_overflow_errors_like_pg() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT 30000::int2 + 30000::int2").is_err());
    // INT2_MIN / -1 has no representable int2 quotient.
    assert!(e.execute("SELECT (-32768)::int2 / (-1)::int2").is_err());
}

// v7.39 (read01 int.c/int8.c) — overflow/error surfaces byte-locked vs
// PG18: canonical out-of-range texts on every width (22003 on the wire),
// division by zero (22012), INT_MIN specials, gcd/lcm overflow, the
// integer input-syntax wording, and generate_series' zero step.
#[test]
fn overflow_and_error_surfaces_match_pg() {
    let mut e = Engine::new();
    let err = |e: &mut Engine, sql: &str| -> String {
        format!("{}", e.execute(sql).unwrap_err())
    };
    assert!(err(&mut e, "SELECT 2147483647::int + 1").contains("integer out of range"));
    assert!(err(&mut e, "SELECT 32767::smallint + 1::smallint").contains("smallint out of range"));
    assert!(
        err(&mut e, "SELECT 9223372036854775807::bigint + 1").contains("bigint out of range")
    );
    assert!(err(&mut e, "SELECT (-2147483648)::int / (-1)").contains("integer out of range"));
    assert!(
        err(&mut e, "SELECT (-9223372036854775808)::bigint / (-1)")
            .contains("bigint out of range")
    );
    assert!(err(&mut e, "SELECT abs((-2147483648)::int)").contains("integer out of range"));
    // INT_MIN % -1 is 0, not an overflow (PG).
    match e.execute("SELECT (-2147483648)::int % (-1)").unwrap() {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "0");
        }
        other => panic!("{other:?}"),
    }
    assert!(err(&mut e, "SELECT gcd((-2147483648)::int, 0)").contains("integer out of range"));
    assert!(err(&mut e, "SELECT lcm(2147483647::int, 2)").contains("integer out of range"));
    assert!(err(&mut e, "SELECT 65535::int2").contains("smallint out of range"));
    assert!(
        err(&mut e, "SELECT '42abc'::int")
            .contains("invalid input syntax for type integer: \"42abc\"")
    );
    assert!(err(&mut e, "SELECT generate_series(1, 5, 0)").contains("step size cannot equal zero"));
}
