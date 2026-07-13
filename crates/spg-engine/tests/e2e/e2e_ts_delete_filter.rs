//! v7.37.17 (17.6 siblings) — ts_delete / ts_filter / tsquery_phrase.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
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

#[test]
fn ts_delete_removes_lexeme() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT ts_delete('cat:3 dog:7 fish:12', 'dog')"
        )),
        "cat:3 fish:12"
    );
    // Not present — unchanged.
    assert_eq!(
        text(&first(&mut e, "SELECT ts_delete('cat dog', 'bird')")),
        "cat dog"
    );
}

#[test]
fn ts_filter_keeps_weighted() {
    let mut e = Engine::new();
    // Only 'cat:1A' carries weight A.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT ts_filter('cat:1A dog:2B fish:3', '{a}')"
        )),
        "cat:1A"
    );
    // Unweighted positions count as D.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT ts_filter('cat:1A dog:2B fish:3', '{d}')"
        )),
        "fish:3"
    );
    // Two weights.
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT ts_filter('cat:1A dog:2B fish:3', '{a,b}')"
        )),
        "cat:1A dog:2B"
    );
}

#[test]
fn ts_filter_invalid_weight_errors() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT ts_filter('cat:1A', '{x}')").is_err());
}

#[test]
fn tsquery_phrase_joins() {
    // v7.39 (read01 tsquery) — tsquery_phrase returns a tsquery value
    // that renders as PG's canonical phrase form.
    let mut e = Engine::new();
    assert_eq!(
        spg_engine::eval::value_to_text(&first(
            &mut e,
            "SELECT tsquery_phrase('fat', 'cat')"
        )),
        "'fat' <-> 'cat'"
    );
    assert_eq!(
        spg_engine::eval::value_to_text(&first(
            &mut e,
            "SELECT tsquery_phrase('fat', 'cat', 10)"
        )),
        "'fat' <10> 'cat'"
    );
}

#[test]
fn ts_ops_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "ts_delete(NULL::text, 'a')",
        "ts_delete('a', NULL::text)",
        "ts_filter(NULL::text, '{a}')",
        "tsquery_phrase(NULL::text, 'b')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
