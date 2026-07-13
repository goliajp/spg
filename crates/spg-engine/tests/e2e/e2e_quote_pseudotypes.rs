//! v7.39 (read01 utils/adt, round 23) — quote.c (zero-delta lock) +
//! pseudotypes.c (::cstring identity I/O, pseudotype input rejection).
//! Byte-locked vs PG18.

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
fn quote_family_locked() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT quote_ident('foo'), quote_ident('Foo'), quote_ident('foo\"bar'), \
             quote_ident('select'), quote_ident('1abc'), quote_ident('_ok'), quote_ident('')"
        ),
        vec!["foo", "\"Foo\"", "\"foo\"\"bar\"", "\"select\"", "\"1abc\"", "_ok", "\"\""]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT quote_literal('it''s'), quote_literal('a\\b'), quote_literal(42), \
             quote_nullable(NULL), quote_nullable(7)"
        ),
        vec!["'it''s'", "E'a\\\\b'", "'42'", "NULL", "'7'"]
    );
}

#[test]
fn cstring_identity_and_pseudotype_rejection() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(&mut e, "SELECT 'a'::cstring, 'x'::text::cstring::text"),
        vec!["a", "x"]
    );
    // NULL passes through any pseudotype cast; a value hits the dummy
    // input function.
    assert_eq!(row_of(&mut e, "SELECT NULL::anyarray"), vec!["NULL"]);
    let err = e.execute("SELECT '{1}'::anyarray").unwrap_err();
    assert!(
        format!("{err}").contains("cannot accept a value of type anyarray"),
        "{err}"
    );
    let err = e.execute("SELECT 1::anyelement").unwrap_err();
    assert!(
        format!("{err}").contains("cannot accept a value of type anyelement"),
        "{err}"
    );
}
