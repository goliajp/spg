//! read01 round 325 (V57) — array / bytea / bit input errors, and the
//! declared bit width.
//!
//! The headline is not a message: a `BIT(3)` column ACCEPTED a two-bit
//! string literal and stored it two bits wide. The width was enforced on
//! the `B'…'` bit-literal path (round 281) and not on the text path, so a
//! column that promises a fixed width quietly held another one. PG 18.4:
//! `bit string length 2 does not match type bit(3)`.
//!
//! The wording work around it, all measured on PG 18.4:
//!
//!   * a bad array literal is `malformed array literal: "…"` with one of
//!     PG's DETAIL lines — and the coercion path used to answer SPG's own
//!     `cannot parse "abc" as INT[]: TEXT[] literal must be enclosed in
//!     '{...}'`, naming TEXT[] for an INT[] column and disagreeing with
//!     what the CAST path said for the very same input;
//!   * an element that will not convert is the ELEMENT type's own error;
//!   * bytea reports `invalid hexadecimal digit: "Z"` /
//!     `invalid hexadecimal data: odd number of digits`.
//!
//! Two of those DETAILs required fixing the decoder, not the wording:
//! `{1,2}}` was read as the elements `1` and `2}` (one brace peeled off
//! each end), and `{1,}` as an empty element.

use spg_engine::Engine;

fn err(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql) {
        Ok(v) => panic!("{sql}: expected an error, got {v:?}"),
        Err(x) => format!("{x}")
            .trim_start_matches("eval: ")
            .trim_start_matches("unsupported: ")
            .trim_start_matches("type mismatch: ")
            .to_string(),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t (x INT[], y TEXT[], z BYTEA, b BIT(3), v VARBIT(3))")
        .unwrap();
    e
}

/// The declared width binds on the text path too. This is the one that
/// stored a wrong-width value rather than erroring.
#[test]
fn a_bit_column_enforces_its_width_against_a_string_literal() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "INSERT INTO t (b) VALUES ('10')"),
        "bit string length 2 does not match type bit(3)",
    );
    assert_eq!(
        err(&mut e, "INSERT INTO t (b) VALUES ('1010')"),
        "bit string length 4 does not match type bit(3)",
    );
    // The exact width still goes in.
    e.execute("INSERT INTO t (b) VALUES ('101')").unwrap();

    // BIT VARYING is a maximum, not an exact width.
    e.execute("INSERT INTO t (v) VALUES ('10')").unwrap();
    assert_eq!(
        err(&mut e, "INSERT INTO t (v) VALUES ('1010')"),
        "bit string too long for type bit varying(3)",
    );
}

/// PG's array DETAIL lines, on the coercion path.
#[test]
fn a_malformed_array_literal_carries_pgs_detail() {
    let mut e = fixture();
    for (lit, detail) in [
        (
            "abc",
            "Array value must start with \"{\" or dimension information.",
        ),
        ("{1,2", "Unexpected end of input."),
        ("{1,2}}", "Junk after closing right brace."),
        ("{1,2}x", "Junk after closing right brace."),
        ("{1,}", "Unexpected \"}\" character."),
    ] {
        assert_eq!(
            err(&mut e, &format!("INSERT INTO t (x) VALUES ('{lit}')")),
            format!("malformed array literal: \"{lit}\" DETAIL: {detail}"),
            "for `{lit}`"
        );
    }
}

/// An element that will not convert is the ELEMENT type's error, not a
/// malformed-literal one — that is how PG splits the two.
#[test]
fn a_bad_element_is_the_element_types_own_error() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, "INSERT INTO t (x) VALUES ('{x}')"),
        "invalid input syntax for type integer: \"x\"",
    );
}

/// The CAST path and the coercion path answer the same thing now.
#[test]
fn cast_and_coercion_agree_on_a_bad_array() {
    let mut e = fixture();
    let expected =
        "malformed array literal: \"abc\" DETAIL: Array value must start with \"{\" or dimension information.";
    assert_eq!(err(&mut e, "SELECT 'abc'::int[]"), expected);
    assert_eq!(err(&mut e, "INSERT INTO t (x) VALUES ('abc')"), expected);
    // …including bigint[], whose message began with a stray `BIG`.
    assert_eq!(err(&mut e, "SELECT 'abc'::bigint[]"), expected);
}

#[test]
fn bytea_uses_pgs_hex_errors() {
    let mut e = fixture();
    assert_eq!(
        err(&mut e, r"INSERT INTO t (z) VALUES ('\xZZ')"),
        "invalid hexadecimal digit: \"Z\"",
    );
    assert_eq!(
        err(&mut e, r"INSERT INTO t (z) VALUES ('\x1')"),
        "invalid hexadecimal data: odd number of digits",
    );
}

/// The shapes that are still legal must stay legal.
#[test]
fn well_formed_literals_still_load() {
    let mut e = fixture();
    e.execute("INSERT INTO t (x, y) VALUES ('{1,2,3}', '{a,\"b,c\",NULL}')")
        .unwrap();
    e.execute("INSERT INTO t (x) VALUES ('{}')").unwrap();
    e.execute(r"INSERT INTO t (z) VALUES ('\x0a0b')").unwrap();
}
