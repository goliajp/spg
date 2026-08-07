//! v7.39 (round 271) — NUMERIC's display scale is a u16.
//!
//! At u8 a decimal with more than 255 places could not be represented
//! at all. The literal path silently handed such a value to the float
//! lexer (so `pg_typeof(1e-256)` answered double precision), and a
//! plain 256-place decimal reached a converter whose
//! `.expect("lexer-validated decimal")` aborted the query with an
//! internal error — on SQL PG accepts.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    assert_eq!(rows.len(), 1, "{sql}");
    spg_engine::eval::value_to_text(&rows[0].values[0])
}

/// `0.` followed by `zeros` zeros and a final 1 — i.e. 1e-(zeros+1).
fn deep_decimal(zeros: usize) -> String {
    let mut s = String::from("0.");
    for _ in 0..zeros {
        s.push('0');
    }
    s.push('1');
    s
}

#[test]
fn a_literal_past_255_decimal_places_stays_numeric() {
    let mut e = Engine::new();
    // The u8 boundary was exactly here: 1e-255 was numeric, 1e-256 was
    // not. PG calls both numeric.
    assert_eq!(one(&mut e, "SELECT pg_typeof(1e-255)"), "numeric");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1e-256)"), "numeric");
    assert_eq!(one(&mut e, "SELECT pg_typeof(1e-400)"), "numeric");
    assert_eq!(one(&mut e, "SELECT scale(1e-256)"), "256");
}

#[test]
fn a_plain_256_place_decimal_no_longer_aborts_the_query() {
    let mut e = Engine::new();
    // This exact statement used to come back as
    // "internal error: query aborted by internal error: lexer-validated
    // decimal".
    let lit = deep_decimal(255);
    assert_eq!(one(&mut e, &format!("SELECT {lit}")), lit);
    assert_eq!(one(&mut e, &format!("SELECT pg_typeof({lit})")), "numeric",);
}

#[test]
fn arithmetic_at_a_deep_scale_stays_exact() {
    let mut e = Engine::new();
    // PG 18.4 keeps every digit: 1 followed by a 256-place fraction.
    let want = {
        let mut s = String::from("1.");
        for _ in 0..255 {
            s.push('0');
        }
        s.push('1');
        s
    };
    assert_eq!(one(&mut e, "SELECT 1e-256 + 1"), want);
    assert_eq!(one(&mut e, "SELECT pg_typeof(1e-256 + 1)"), "numeric");
}

#[test]
fn rounding_to_a_deep_scale_stays_numeric() {
    let mut e = Engine::new();
    // round(x, n) with n past what i128 can carry used to fall through
    // to the f64 tail and hand back 9.999999999999994e-257.
    // PG pads out to the requested scale: 255 zeros, the 1, then 44
    // more zeros — a 302-character text, measured.
    let mut want = deep_decimal(255);
    for _ in 0..44 {
        want.push('0');
    }
    assert_eq!(one(&mut e, "SELECT round(1e-256, 300)"), want);
    assert_eq!(one(&mut e, "SELECT scale(round(1e-256, 300))"), "300");
    assert_eq!(
        one(&mut e, "SELECT pg_typeof(round(1e-256, 300))"),
        "numeric",
    );
}

#[test]
fn converting_a_deep_numeric_to_float_is_correctly_rounded() {
    let mut e = Engine::new();
    // The conversion built its divisor with repeated multiplication,
    // which accumulated error (1e-300 came out 9.999999999999999e-301)
    // and ran to infinity for a large enough scale — which then looked
    // like an underflow and errored.
    assert_eq!(one(&mut e, "SELECT 1e-300::float8"), "1e-300");
    assert_eq!(one(&mut e, "SELECT 1e-320::float8"), "1e-320");
    // Mixed float/numeric arithmetic converts the numeric to float the
    // way PG does, so the two sides cancel exactly.
    assert_eq!(one(&mut e, "SELECT 1e-300::float8 - 1e-300"), "0");
    assert_eq!(one(&mut e, "SELECT 1e-300::float8 + 1e-300"), "2e-300");
    // Ordinary mixed arithmetic is unchanged.
    assert_eq!(
        one(&mut e, "SELECT 0.1::float8 + 0.2"),
        "0.30000000000000004",
    );
}

#[test]
fn a_deep_numeric_survives_a_round_trip_through_a_column() {
    let mut e = Engine::new();
    // The on-disk forms were extended rather than changed: a scale that
    // fits a byte still writes the old shape, so nothing already
    // persisted moves.
    e.execute("CREATE TABLE t (v numeric)").unwrap();
    e.execute("INSERT INTO t VALUES (1e-256), (0.25), (1e-400)")
        .unwrap();
    assert_eq!(
        one(&mut e, "SELECT scale(v) FROM t ORDER BY v LIMIT 1"),
        "400"
    );
    assert_eq!(
        one(&mut e, "SELECT count(*) FROM t WHERE scale(v) > 255"),
        "2",
    );
}
