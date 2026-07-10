//! v7.37.17 (17.6 siblings) — XML probes.

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

fn as_bool(v: &spg_storage::Value<'_>) -> bool {
    match v {
        spg_storage::Value::Bool(b) => *b,
        other => panic!("expected Bool, got {other:?}"),
    }
}

#[test]
fn xmlcomment_wraps_text() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT xmlcomment('hello')")),
        "<!--hello-->"
    );
    assert_eq!(text(&first(&mut e, "SELECT xmlcomment('')")), "<!---->");
}

#[test]
fn xmlcomment_rejects_dashes() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT xmlcomment('bad -- middle')").is_err());
}

#[test]
fn xml_is_well_formed_shape_heuristic() {
    let mut e = Engine::new();
    assert!(as_bool(&first(
        &mut e,
        "SELECT xml_is_well_formed('<a></a>')"
    )));
    assert!(as_bool(&first(&mut e, "SELECT xml_is_well_formed('')")));
    assert!(!as_bool(&first(
        &mut e,
        "SELECT xml_is_well_formed('not xml')"
    )));
    // Well-formed nesting.
    assert!(as_bool(&first(
        &mut e,
        "SELECT xml_is_well_formed('<a><b></b></a>')"
    )));
    // Note: SPG's heuristic uses only tag-count balance and can
    // report false positives for malformed-nesting cases like
    // '<a><b></a>'. Real DOM-based validation queues with the
    // XML crate epic.
}

#[test]
fn xpath_returns_null() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT xpath('/a', '<a/>')"),
        spg_storage::Value::Null
    ));
}

#[test]
fn xpath_exists_returns_false() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT xpath_exists('/a', '<a/>')") {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn xml_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT xmlcomment(NULL::text)"),
        spg_storage::Value::Null
    ));
    assert!(matches!(
        first(&mut e, "SELECT xml_is_well_formed(NULL::text)"),
        spg_storage::Value::Null
    ));
}
