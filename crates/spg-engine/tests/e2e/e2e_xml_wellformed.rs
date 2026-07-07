//! v7.38 (read01 P6.38) — `::xml` (PG CONTENT mode) validates well-formedness:
//! balanced, properly-nested tags. Plain text, multiple roots, comments and
//! self-closing tags are accepted; unclosed / mismatched tags are rejected.
//! Oracle behaviour from live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn ok(e: &mut Engine, sql: &str) -> bool {
    matches!(e.execute(sql), Ok(QueryResult::Rows { .. }))
}

#[test]
fn well_formed_xml_is_accepted() {
    let mut e = Engine::new();
    assert!(ok(&mut e, "SELECT '<a>ok</a>'::xml"));
    assert!(ok(&mut e, "SELECT '<a><b>n</b></a>'::xml"));
    assert!(ok(&mut e, "SELECT '<a>1</a><b>2</b>'::xml")); // content mode: multiple roots
    assert!(ok(&mut e, "SELECT 'plain text'::xml"));
    assert!(ok(&mut e, r#"SELECT '<a attr="x"/>'::xml"#));
    assert!(ok(&mut e, "SELECT '<!-- c --><a/>'::xml"));
    assert!(ok(&mut e, "SELECT ''::xml"));
    // A '>' inside a quoted attribute must not end the tag early.
    assert!(ok(&mut e, r#"SELECT '<a b=">">x</a>'::xml"#));
    // CDATA content is not parsed as markup.
    assert!(ok(&mut e, "SELECT '<![CDATA[<not a tag>]]>'::xml"));
}

#[test]
fn malformed_xml_is_rejected() {
    let mut e = Engine::new();
    assert!(e.execute("SELECT '<a>unclosed'::xml").is_err());
    assert!(e.execute("SELECT '<a></b>'::xml").is_err());
    assert!(e.execute("SELECT '<a><b></a></b>'::xml").is_err()); // improper nesting
    assert!(e.execute("SELECT '<a>'::xml").is_err());
}
