//! v7.39 (read01 utils/adt, round 41) — the single-byte `"char"` type
//! (char.c): int↔"char" round-trip, ascii/bit_length on a "char", and
//! byte-value comparison / ordering. Byte-locked vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn char1_int_round_trip() {
    let mut e = Engine::new();
    // int -> "char" takes the low byte; "char" -> int is the byte value.
    assert_eq!(
        row_of(&mut e, "SELECT 65::\"char\", ('A'::\"char\")::int"),
        vec!["A", "65"]
    );
}

#[test]
fn char1_ascii_and_bit_length() {
    let mut e = Engine::new();
    // ascii of a "char" is its byte value; bit_length of a single byte is 8.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT ascii('z'::\"char\"), bit_length('z'::\"char\")"
        ),
        vec!["122", "8"]
    );
}

#[test]
fn char1_orders_by_byte_value() {
    let mut e = Engine::new();
    // "char" compares/orders by byte value: 'z'(122) > 'a'(97).
    assert_eq!(
        row_of(
            &mut e,
            "SELECT ('z'::\"char\") > ('a'::\"char\"), ('z'::\"char\") < ('a'::\"char\")"
        ),
        vec!["true", "false"]
    );
    match e
        .execute(
            "SELECT c FROM (VALUES ('m'::\"char\"), ('a'::\"char\"), ('z'::\"char\")) v(c) \
             ORDER BY c",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect();
            assert_eq!(got, vec!["a", "m", "z"]);
        }
        other => panic!("order: {other:?}"),
    }
}
