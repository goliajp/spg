//! v7.37.17 (17.6 siblings) — tsvector_to_array / array_to_tsvector
//! / get_current_ts_config / ts_lexize.

use spg_engine::{Engine, QueryResult};

fn first(e: &mut Engine, sql: &str) -> spg_storage::Value<'static> {
    let r = e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows");
    };
    rows[0].values[0].clone()
}

fn text_array(v: &spg_storage::Value<'_>) -> Vec<Option<String>> {
    match v {
        spg_storage::Value::TextArray(items) => items.clone(),
        other => panic!("expected TextArray, got {other:?}"),
    }
}

fn text(v: &spg_storage::Value<'_>) -> String {
    match v {
        spg_storage::Value::Text(s) => s.to_string(),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn tsvector_to_array_extracts_sorted_lexemes() {
    let mut e = Engine::new();
    assert_eq!(
        text_array(&first(
            &mut e,
            "SELECT tsvector_to_array('fat:2 cat:3 rat:5A')"
        )),
        vec![
            Some("cat".to_string()),
            Some("fat".to_string()),
            Some("rat".to_string())
        ]
    );
}

#[test]
fn array_to_tsvector_sorts_and_dedups() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(
            &mut e,
            "SELECT array_to_tsvector(ARRAY['fat', 'cat', 'rat', 'cat'])"
        )),
        "cat fat rat"
    );
}

#[test]
fn array_to_tsvector_rejects_nulls() {
    let mut e = Engine::new();
    assert!(e
        .execute("SELECT array_to_tsvector(ARRAY['fat', NULL])")
        .is_err());
}

#[test]
fn get_current_ts_config_returns_english() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT get_current_ts_config()")),
        "english"
    );
}

#[test]
fn ts_lexize_simple_lowercases() {
    let mut e = Engine::new();
    assert_eq!(
        text_array(&first(&mut e, "SELECT ts_lexize('simple', 'PriCE')")),
        vec![Some("price".to_string())]
    );
}

#[test]
fn tsvector_array_null_passthrough() {
    let mut e = Engine::new();
    for f in &[
        "tsvector_to_array(NULL::text)",
        "ts_lexize('simple', NULL::text)",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}
