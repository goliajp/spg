//! v7.38 (read01 P6.31) — querytree() on a tsquery value returns the
//! GIN-indexable part: NOT nodes drop, AND keeps the indexable side, OR is
//! indexable only if both sides are. Oracle behaviour from live PG 18.4.

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
fn querytree_returns_indexable_part() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT querytree('a & b'::tsquery)"),
        "'a' & 'b'"
    );
    // AND with a NOT side keeps only the indexable side.
    assert_eq!(text(&mut e, "SELECT querytree('a & !b'::tsquery)"), "'a'");
    // A bare NOT is not indexable at all.
    assert_eq!(text(&mut e, "SELECT querytree('!a'::tsquery)"), "T");
    assert_eq!(
        text(&mut e, "SELECT querytree('a | b'::tsquery)"),
        "'a' | 'b'"
    );
    // OR with a NOT side is entirely non-indexable.
    assert_eq!(text(&mut e, "SELECT querytree('a | !b'::tsquery)"), "T");
}
