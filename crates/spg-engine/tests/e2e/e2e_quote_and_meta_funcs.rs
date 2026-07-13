//! v7.37.17 (17.6 siblings) — quote_ident / quote_literal /
//! quote_nullable / format_type / obj_description / to_reg* /
//! pg_client_encoding / pg_is_in_recovery.

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

#[test]
fn quote_ident_wraps_in_double_quotes() {
    let mut e = Engine::new();
    // PG's quote_ident leaves a safe unquoted identifier verbatim.
    // (Live PG18: `quote_ident('foo')` = `foo`, not `"foo"`.)
    match first(&mut e, "SELECT quote_ident('foo')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "foo"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT quote_ident('foo\"bar')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "\"foo\"\"bar\""),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn quote_literal_wraps_in_single_quotes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT quote_literal('bar')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "'bar'"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT quote_literal('it''s')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "'it''s'"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn quote_nullable_returns_null_text_for_null() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT quote_nullable(NULL)") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "NULL"),
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT quote_nullable('x')") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "'x'"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn to_regclass_missing_relation_is_null() {
    // Upgraded from stub: to_regclass/to_regtype are real
    // resolvers now (see e2e_to_regclass.rs); a missing relation
    // still resolves to NULL, and 'int' resolves to oid 23.
    let mut e = Engine::new();
    assert!(matches!(
        first(&mut e, "SELECT to_regclass('some_table')"),
        spg_storage::Value::Null
    ));
    // v7.39 (read01 regproc.c) — to_regtype renders the canonical name.
    assert_eq!(
        first(&mut e, "SELECT to_regtype('int')"),
        spg_storage::Value::text("integer")
    );
}

/// U24 (read01 A-group): quote_literal / quote_nullable on a
/// non-text argument used to leak a Rust debug dump
/// (`quote_literal(42)` → `'Int(42)'`). PG performs the
/// anyelement→text cast then literal-quotes. Every expected value
/// below was captured live from PG 18.4 (`psql -tAc`).
#[test]
fn quote_literal_non_text_matches_pg18() {
    let mut e = Engine::new();
    let q = |e: &mut Engine, sql: &str| match first(e, sql) {
        spg_storage::Value::Text(s) => s.to_string(),
        spg_storage::Value::Null => "NULL".to_string(),
        other => panic!("{sql}: got {other:?}"),
    };
    // Numbers / bool / temporal — no debug leak.
    assert_eq!(q(&mut e, "SELECT quote_literal(42)"), "'42'");
    assert_eq!(q(&mut e, "SELECT quote_literal(3.14)"), "'3.14'");
    assert_eq!(
        q(&mut e, "SELECT quote_literal(12345678901234::bigint)"),
        "'12345678901234'"
    );
    assert_eq!(q(&mut e, "SELECT quote_literal(1.5::numeric)"), "'1.5'");
    // bool renders as the ::text cast (true/false), not the t/f wire form.
    assert_eq!(q(&mut e, "SELECT quote_literal(true)"), "'true'");
    assert_eq!(q(&mut e, "SELECT quote_literal(false)"), "'false'");
    assert_eq!(
        q(&mut e, "SELECT quote_literal(DATE '2024-01-15')"),
        "'2024-01-15'"
    );
    assert_eq!(
        q(
            &mut e,
            "SELECT quote_literal(TIMESTAMP '2024-01-15 10:30:00')"
        ),
        "'2024-01-15 10:30:00'"
    );
    // Backslash triggers PG's E'…' escape-string form (both text and
    // non-text inputs). PG18: quote_literal('c:\p') = E'c:\\p'.
    assert_eq!(
        q(&mut e, "SELECT quote_literal('c:\\path')"),
        "E'c:\\\\path'"
    );
    assert_eq!(q(&mut e, "SELECT quote_literal('a\\b''c')"), "E'a\\\\b''c'");
    // Embedded quote still doubled without backslash.
    assert_eq!(q(&mut e, "SELECT quote_literal('a''b')"), "'a''b'");
    // quote_nullable mirrors quote_literal for non-null, NULL→'NULL'.
    assert_eq!(q(&mut e, "SELECT quote_nullable(42)"), "'42'");
    assert_eq!(q(&mut e, "SELECT quote_nullable(true)"), "'true'");
    assert_eq!(q(&mut e, "SELECT quote_nullable(NULL::int)"), "NULL");
}

#[test]
fn recovery_and_encoding_probes() {
    let mut e = Engine::new();
    match first(&mut e, "SELECT pg_is_in_recovery()") {
        spg_storage::Value::Bool(false) => {}
        other => panic!("got {other:?}"),
    }
    match first(&mut e, "SELECT pg_client_encoding()") {
        spg_storage::Value::Text(s) => assert_eq!(s.as_ref(), "UTF8"),
        other => panic!("got {other:?}"),
    }
}
