//! v7.37.17 (17.6 siblings) — FTS introspection: strip /
//! tsvector_length / numnode / querytree.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

fn as_int(v: &spg_storage::Value<'_>) -> i32 {
    match v {
        spg_storage::Value::Int(n) => *n,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn strip_removes_positions() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT strip('cat:3 dog:7 fish:12')")),
        "cat dog fish"
    );
    // Already stripped — unchanged.
    assert_eq!(text(&first(&mut e, "SELECT strip('cat dog')")), "cat dog");
}

#[test]
fn tsvector_length_counts_lexemes() {
    let mut e = Engine::new();
    assert_eq!(
        as_int(&first(&mut e, "SELECT tsvector_length('cat:3 dog:7 fish:12')")),
        3
    );
    assert_eq!(as_int(&first(&mut e, "SELECT tsvector_length('')")), 0);
}

#[test]
fn numnode_counts_nodes() {
    let mut e = Engine::new();
    // 'cat & dog' → 2 words + 1 operator = 3 nodes.
    assert_eq!(as_int(&first(&mut e, "SELECT numnode('cat & dog')")), 3);
    // Single word → 1 node.
    assert_eq!(as_int(&first(&mut e, "SELECT numnode('cat')")), 1);
}

#[test]
fn querytree_drops_negated() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT querytree('cat !dog')")),
        "cat"
    );
    // All-negated → 'T' (matches everything indexably).
    assert_eq!(text(&first(&mut e, "SELECT querytree('!dog')")), "T");
}

#[test]
fn fts_introspection_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "strip(NULL::text)",
        "tsvector_length(NULL::text)",
        "numnode(NULL::text)",
        "querytree(NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
