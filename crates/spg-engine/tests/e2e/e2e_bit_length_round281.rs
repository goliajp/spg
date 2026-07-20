//! v7.39 (round 281) — BIT(n) / BIT VARYING(n) enforce their length.
//!
//! The typmod was parsed and dropped, so a `bit(3)` column accepted a
//! five-bit string. PG draws two distinctions this pins:
//!
//!   * BIT is FIXED — a SHORTER value is an error too, not just a
//!     longer one — while BIT VARYING is a maximum.
//!   * assignment ENFORCES, an explicit cast ADJUSTS. That is the same
//!     split the varchar arms already model, and the casts were
//!     already right; only the column side was missing.
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

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}")
            .replace("unsupported: ", "")
            .replace("eval: type mismatch: ", ""),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE bt (a bit(3), b bit varying(3), c bit, d varbit)")
        .unwrap();
    e
}

#[test]
fn a_fixed_bit_column_requires_the_exact_length() {
    let mut e = fixture();
    e.execute("INSERT INTO bt (a) VALUES (B'101')").unwrap();
    assert_eq!(
        err(&mut e, "INSERT INTO bt (a) VALUES (B'10101')"),
        "bit string length 5 does not match type bit(3)",
    );
    // Shorter is an error too — this is the half that separates BIT
    // from BIT VARYING.
    assert_eq!(
        err(&mut e, "INSERT INTO bt (a) VALUES (B'1')"),
        "bit string length 1 does not match type bit(3)",
    );
}

#[test]
fn a_varying_column_treats_it_as_a_maximum() {
    let mut e = fixture();
    e.execute("INSERT INTO bt (b) VALUES (B'101')").unwrap();
    e.execute("INSERT INTO bt (b) VALUES (B'1')").unwrap();
    assert_eq!(
        err(&mut e, "INSERT INTO bt (b) VALUES (B'10101')"),
        "bit string too long for type bit varying(3)",
    );
}

#[test]
fn a_bare_bit_is_bit_one() {
    let mut e = fixture();
    e.execute("INSERT INTO bt (c) VALUES (B'1')").unwrap();
    assert_eq!(
        err(&mut e, "INSERT INTO bt (c) VALUES (B'11')"),
        "bit string length 2 does not match type bit(1)",
    );
}

#[test]
fn a_bare_varbit_is_unbounded() {
    let mut e = fixture();
    e.execute("INSERT INTO bt (d) VALUES (B'1010101010101')")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT length(d) FROM bt"), "13");
}

#[test]
fn an_explicit_cast_adjusts_instead_of_erroring() {
    let mut e = Engine::new();
    // bit(n) truncates AND pads.
    assert_eq!(one(&mut e, "SELECT B'10101'::bit(3)"), "101");
    assert_eq!(one(&mut e, "SELECT B'1'::bit(3)"), "100");
    assert_eq!(one(&mut e, "SELECT B'101'::bit(5)"), "10100");
    assert_eq!(one(&mut e, "SELECT length(B'101'::bit(5))"), "5");
    // varbit(n) truncates but never pads — and `bit varying(3)` as a
    // cast target used to be a parse error outright.
    assert_eq!(one(&mut e, "SELECT B'10101'::bit varying(3)"), "101");
    assert_eq!(one(&mut e, "SELECT B'1'::bit varying(3)"), "1");
}

#[test]
fn the_declared_length_survives_a_catalog_round_trip() {
    let mut e = fixture();
    let bytes = e.catalog().serialize();
    let mut restored = Engine::restore_envelope(&bytes).expect("reload");
    // The length is only on disk from this round; a reload that lost it
    // would accept the five-bit string again.
    assert_eq!(
        err(&mut restored, "INSERT INTO bt (a) VALUES (B'10101')"),
        "bit string length 5 does not match type bit(3)",
    );
}
