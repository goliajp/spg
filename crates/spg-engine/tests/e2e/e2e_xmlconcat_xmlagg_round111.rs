//! v7.39 (read01 round 111) — xmlconcat / xmlagg over xml values.
//!
//! `xmlconcat('<a/>'::xml, '<b/>'::xml)` and `xmlagg(x)` over xml rows both
//! errored ("needs xml text" / "string_agg requires text value") — neither the
//! xmlconcat handler nor the shared StringAgg path had an `xml` arm. Both now
//! accept xml, and xmlconcat returns `xml` (so nesting it in xmlelement inlines
//! the fragment instead of escaping it). Locked byte-identical against PG 18.4.

use spg_engine::{Engine, QueryResult};

fn text(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => match &rows[0].values[0] {
            spg_storage::Value::Null => "NULL".to_string(),
            v => spg_engine::eval::value_to_text(v),
        },
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn xmlconcat_over_xml_values() {
    let mut e = Engine::new();
    assert_eq!(
        text(&mut e, "SELECT xmlconcat('<a/>'::xml, '<b/>'::xml)::text"),
        "<a/><b/>"
    );
    // NULL fragments are skipped.
    assert_eq!(
        text(
            &mut e,
            "SELECT xmlconcat('<a/>'::xml, NULL, '<b/>'::xml)::text"
        ),
        "<a/><b/>"
    );
    // All-NULL yields NULL.
    assert_eq!(
        text(&mut e, "SELECT xmlconcat(NULL::xml, NULL::xml)::text"),
        "NULL"
    );
    // The result is xml, so nesting it inlines the fragment (not escaped).
    assert_eq!(
        text(
            &mut e,
            "SELECT xmlelement(name root, xmlconcat('<a/>'::xml, '<b/>'::xml))::text"
        ),
        "<root><a/><b/></root>"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT pg_typeof(xmlconcat('<a/>'::xml, '<b/>'::xml))::text"
        ),
        "xml"
    );
}

#[test]
fn xmlagg_over_xml_rows() {
    let mut e = Engine::new();
    assert_eq!(
        text(
            &mut e,
            "SELECT xmlagg(x)::text FROM (VALUES('<a/>'::xml),('<b/>'::xml)) t(x)"
        ),
        "<a/><b/>"
    );
    assert_eq!(
        text(
            &mut e,
            "SELECT xmlagg(x ORDER BY id DESC)::text FROM (VALUES(1,'<a/>'::xml),(2,'<b/>'::xml)) t(id,x)"
        ),
        "<b/><a/>"
    );
}
