//! v7.39 (read01 utils/adt, round 42) — the `name` identifier type
//! (name.c: byte-value comparison / ordering / length / casts, all
//! aligned with PG18 out of the box) and the 3-arg `convert(bytea,
//! src, dst)` encoding transcode (mbutils.c), which composes the
//! single-byte tables already backing convert_from / convert_to.

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
fn name_compare_length_cast() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT 'b'::name < 'a'::name, 'a'::name < 'b'::name, length('abc'::name), \
             ('abc'::name)::text, ('abc'::text)::name"
        ),
        vec!["false", "true", "3", "abc", "abc"]
    );
    // name truncates at 63 bytes (NAMEDATALEN-1), no blank padding.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT length(('abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABCDEFGHIJ')::name)"
        ),
        vec!["63"]
    );
}

#[test]
fn name_orders_by_byte_value() {
    let mut e = Engine::new();
    match e
        .execute(
            "SELECT c FROM (VALUES ('m'::name), ('a'::name), ('z'::name), ('A'::name)) v(c) \
             ORDER BY c",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            let got: Vec<String> = rows
                .iter()
                .map(|r| spg_engine::eval::value_to_text(&r.values[0]))
                .collect();
            // C-locale byte order: 'A'(65) < 'a'(97) < 'm'(109) < 'z'(122).
            assert_eq!(got, vec!["A", "a", "m", "z"]);
        }
        other => panic!("order: {other:?}"),
    }
}

#[test]
fn convert_transcodes_encodings() {
    let mut e = Engine::new();
    // UTF8->UTF8 is identity; café->LATIN1 folds é to a single 0xe9 byte;
    // the round-trip back to UTF8 restores the 2-byte c3a9 sequence.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT convert('abc', 'UTF8', 'UTF8'), \
             convert('café', 'UTF8', 'LATIN1'), \
             convert(convert('café', 'UTF8', 'LATIN1'), 'LATIN1', 'UTF8')"
        ),
        vec!["\\x616263", "\\x636166e9", "\\x636166c3a9"]
    );
}
