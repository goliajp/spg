//! v7.38 (read01 P6.30) — strip() and ts_delete() accept a real tsvector value
//! (not just the text form). Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Text(s) => s.to_string(),
            v => format!("{v:?}"),
        },
        _ => panic!("expected rows"),
    }
}

#[test]
fn strip_removes_positions_from_tsvector() {
    let mut e = Engine::new();
    // PG: strip('a:1 b:2,3') → 'a' 'b'
    assert_eq!(
        text(&mut e, "SELECT strip('a:1 b:2,3'::tsvector)::text"),
        "'a' 'b'"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT strip(to_tsvector('simple','hello world'))::text"
        ),
        "'hello' 'world'"
    );
}

#[test]
fn ts_delete_removes_lexemes_from_tsvector() {
    let mut e = Engine::new();
    // PG: ts_delete('a:1 b:2 c:3', 'b') → 'a':1 'c':3
    assert_eq!(
        text(
            &mut e,
            "SELECT ts_delete('a:1 b:2 c:3'::tsvector, 'b')::text"
        ),
        "'a':1 'c':3"
    );
    // Array form removes several lexemes at once.
    assert_eq!(
        text(
            &mut e,
            "SELECT ts_delete('a:1 b:2 c:3'::tsvector, ARRAY['a','c'])::text"
        ),
        "'b':2"
    );
}
