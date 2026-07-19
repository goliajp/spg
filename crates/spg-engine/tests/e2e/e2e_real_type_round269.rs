//! v7.39 (round 269) — REAL / FLOAT4 is a 32-bit type.
//!
//! `real` and `float4` column declarations mapped to SPG's 64-bit
//! FLOAT, on a comment that said "DOUBLE / REAL are 64-bit IEEE — same
//! as our FLOAT". The width is observable: a real column holding 0.1
//! stored the f64 0.1, so `r = 0.1::real` answered false where PG
//! answers true. Everything else was already in place — Value::Real,
//! the storage codec's tag 66, OID 700, the wire type — only the
//! column-type mapping was missing.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn lines(e: &mut Engine, sql: &str) -> Vec<String> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows.into_iter()
        .map(|row| {
            row.values
                .iter()
                .map(|v| match v {
                    spg_storage::Value::Null => String::new(),
                    // Expectations are transcribed from `psql -tA`,
                    // which spells booleans t/f.
                    spg_storage::Value::Bool(b) => {
                        String::from(if *b { "t" } else { "f" })
                    }
                    other => spg_engine::eval::value_to_text(other),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(_) => panic!("{sql}: expected an error"),
        Err(x) => format!("{x}"),
    }
}

#[test]
fn a_real_column_is_32_bit() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (r real, f float8)").unwrap();
    e.execute("INSERT INTO t VALUES (0.1, 0.1)").unwrap();
    assert_eq!(
        lines(&mut e, "SELECT pg_typeof(r), pg_typeof(f) FROM t"),
        vec!["real|double precision"],
    );
    // The point of the whole round: the column narrows to f32, so it
    // compares equal to a real literal. It answered false before.
    assert_eq!(lines(&mut e, "SELECT r = 0.1::real FROM t"), vec!["t"]);
    assert_eq!(lines(&mut e, "SELECT r, f FROM t"), vec!["0.1|0.1"]);
}

#[test]
fn every_spelling_of_the_two_widths() {
    let mut e = Engine::new();
    // PG: float(1..24) is real, float(25..53) is double precision.
    e.execute(
        "CREATE TABLE t (r real, f4 float4, f float8, d double precision, \
         f1 float(1), f24 float(24), f25 float(25), f53 float(53))",
    )
    .unwrap();
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, data_type, udt_name FROM information_schema.columns \
             WHERE table_name = 't' ORDER BY ordinal_position",
        ),
        vec![
            "r|real|float4",
            "f4|real|float4",
            "f|double precision|float8",
            "d|double precision|float8",
            "f1|real|float4",
            "f24|real|float4",
            "f25|double precision|float8",
            "f53|double precision|float8",
        ],
    );
}

#[test]
fn float_precision_bounds_are_rejected_the_way_pg_words_them() {
    let mut e = Engine::new();
    // PG words the two ends differently.
    assert!(
        err(&mut e, "CREATE TABLE a (x float(54))")
            .contains("precision for type float must be less than 54 bits"),
        "{}",
        err(&mut e, "CREATE TABLE a (x float(54))"),
    );
}

#[test]
fn precision_and_radix_are_reported() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (i int, b bigint, n numeric(8,2), r real, f float8, s text)")
        .unwrap();
    // Live PG 18.4. numeric states its precision in decimal digits;
    // everything else states it in bits, hence radix 2 vs 10.
    assert_eq!(
        lines(
            &mut e,
            "SELECT column_name, numeric_precision, numeric_scale, numeric_precision_radix \
             FROM information_schema.columns WHERE table_name = 't' ORDER BY ordinal_position",
        ),
        vec![
            "i|32|0|2",
            "b|64|0|2",
            "n|8|2|10",
            "r|24||2",
            "f|53||2",
            "s|||",
        ],
    );
}

#[test]
fn casts_into_real_round_to_f32() {
    let mut e = Engine::new();
    // 16777217 is the first integer f32 cannot represent.
    assert_eq!(lines(&mut e, "SELECT 16777217::real"), vec!["1.6777216e+07"]);
    assert_eq!(lines(&mut e, "SELECT 0.1::real"), vec!["0.1"]);
    assert_eq!(lines(&mut e, "SELECT '0.1'::real"), vec!["0.1"]);
    assert_eq!(lines(&mut e, "SELECT 12345.678::real"), vec!["12345.678"]);
    // Past the i128 range a NUMERIC literal takes the wide path, which
    // had no route to real at all and surfaced an internal storage
    // mismatch instead.
    assert_eq!(lines(&mut e, "SELECT 1.8e38::real"), vec!["1.8e+38"]);
    assert_eq!(lines(&mut e, "SELECT 3.4e38::real"), vec!["3.4e+38"]);
    assert_eq!(lines(&mut e, "SELECT 1.7e38::real"), vec!["1.7e+38"]);
}

#[test]
fn overflowing_the_f32_range_is_an_error_not_an_infinity() {
    let mut e = Engine::new();
    // All three of these used to hand back Infinity, so a value PG
    // rejects arrived as a live number and every later comparison
    // against it was wrong.
    assert_eq!(
        err(&mut e, "SELECT 1.0e40::real"),
        "eval: type mismatch: \"10000000000000000000000000000000000000000\" is out of range for \
         type real",
    );
    assert_eq!(
        err(&mut e, "SELECT '1e40'::real"),
        "eval: type mismatch: \"1e40\" is out of range for type real",
    );
    // Narrowing a finite double is the one PG words without a quote.
    assert_eq!(
        err(&mut e, "SELECT 1e40::float8::real"),
        "eval: type mismatch: value out of range: overflow",
    );
    // An explicitly written infinity is a value, not an overflow.
    assert_eq!(lines(&mut e, "SELECT 'Infinity'::real"), vec!["Infinity"]);
    assert_eq!(lines(&mut e, "SELECT 'inf'::float8::real"), vec!["Infinity"]);
}

#[test]
fn sum_over_real_stays_real_but_avg_widens() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (r real, f float8)").unwrap();
    e.execute("INSERT INTO t VALUES (1.5, 1.5)").unwrap();
    // PG 18.4: sum(real) is real, avg(real) is double precision, and
    // max keeps the argument type.
    assert_eq!(
        lines(
            &mut e,
            "SELECT pg_typeof(sum(r)), pg_typeof(avg(r)), pg_typeof(max(r)) FROM t",
        ),
        vec!["real|double precision|real"],
    );
    assert_eq!(
        lines(&mut e, "SELECT pg_typeof(sum(f)) FROM t"),
        vec!["double precision"],
    );
    assert_eq!(lines(&mut e, "SELECT sum(r), avg(r) FROM t"), vec!["1.5|1.5"]);
    // A wider value joining the accumulation widens the result.
    e.execute("INSERT INTO t VALUES (2.5, 2.5)").unwrap();
    assert_eq!(lines(&mut e, "SELECT sum(r) FROM t"), vec!["4"]);
}

#[test]
fn arithmetic_keeps_the_narrow_type_where_pg_does() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (r real)").unwrap();
    e.execute("INSERT INTO t VALUES (1.5)").unwrap();
    // PG 18.4: real + int widens to double precision, real * real and
    // negation stay real.
    assert_eq!(
        lines(
            &mut e,
            "SELECT pg_typeof(r + 1), pg_typeof(r * r), pg_typeof(-r) FROM t",
        ),
        vec!["double precision|real|real"],
    );
}
