//! v7.37.17 (17.6 siblings) — xmltext (PG 16) + xmlconcat real
//! implementations + XML export family probes.

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
fn xmltext_escapes_entities() {
    let mut e = Engine::new();
    // PG doc vector: xmltext('< foo & bar >')
    assert_eq!(
        text(&first(&mut e, "SELECT xmltext('< foo & bar >')")),
        "&lt; foo &amp; bar &gt;"
    );
    // Plain text unchanged.
    assert_eq!(text(&first(&mut e, "SELECT xmltext('plain')")), "plain");
}

#[test]
fn xmlconcat_joins_fragments() {
    let mut e = Engine::new();
    assert_eq!(
        text(&first(&mut e, "SELECT xmlconcat('<a/>', '<b/>')")),
        "<a/><b/>"
    );
    // NULL args skipped.
    assert_eq!(
        text(&first(&mut e, "SELECT xmlconcat('<a/>', NULL, '<b/>')")),
        "<a/><b/>"
    );
    // All NULL → NULL.
    assert!(matches!(
        first(&mut e, "SELECT xmlconcat(NULL::text, NULL::text)"),
        spg_storage::Value::Null
    ));
}

#[test]
fn xml_export_family_returns_null() {
    let mut e = Engine::new();
    for f in &[
        "table_to_xml('t', true, false, '')",
        "query_to_xml('SELECT 1', true, false, '')",
        "schema_to_xml('public', true, false, '')",
        "database_to_xml(true, false, '')",
        "table_to_xmlschema('t', true, false, '')",
        "table_to_xml_and_xmlschema('t', true, false, '')",
    ] {
        let sql = format!("SELECT {f}");
        assert!(
            matches!(first(&mut e, &sql), spg_storage::Value::Null),
            "SELECT {f} should be NULL"
        );
    }
}

#[test]
fn xmltext_null_passthrough() {
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT xmltext(NULL::text)"),
        spg_storage::Value::Null
    ));
}
