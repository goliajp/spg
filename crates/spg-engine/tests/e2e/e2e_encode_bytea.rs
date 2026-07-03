//! encode(bytea, format) — PG's standard signature; SPG used to
//! reject a real Bytes value, accepting only text.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    match &rows[0].values[0] {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("{sql}: expected Text, got {other:?}"),
    }
}

#[test]
fn encode_bytea_formats() {
    let mut e = Engine::new();
    assert_eq!(text(&mut e, "SELECT encode('abc'::bytea, 'base64')"), "YWJj");
    assert_eq!(text(&mut e, "SELECT encode('abc'::bytea, 'hex')"), "616263");
    assert_eq!(
        text(&mut e, "SELECT encode('\\xDEADBEEF'::bytea, 'hex')"),
        "deadbeef"
    );
    // Text input still works and matches the bytea form.
    assert_eq!(text(&mut e, "SELECT encode('abc', 'base64')"), "YWJj");
}
