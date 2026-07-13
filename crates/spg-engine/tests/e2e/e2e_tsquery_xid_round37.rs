//! v7.39 (read01 utils/adt, round 37, dir收尾) — tsquery_phrase over
//! tsquery values, and the xid / xid8 casts. Byte-locked vs PG18.

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
fn tsquery_phrase_over_tsquery_values() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT tsquery_phrase('a'::tsquery, 'b'::tsquery), \
             tsquery_phrase('a'::tsquery, 'b'::tsquery, 3)"
        ),
        vec!["'a' <-> 'b'", "'a' <3> 'b'"]
    );
}

#[test]
fn xid_and_xid8_casts() {
    let mut e = Engine::new();
    // xid8 is 64-bit (past the 32-bit xid range), rendered verbatim.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '4294967300'::xid8, '100'::xid, '5'::xid8 < '10'::xid8"
        ),
        vec!["4294967300", "100", "true"]
    );
}
