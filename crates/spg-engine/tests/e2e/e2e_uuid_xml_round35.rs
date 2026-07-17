//! v7.39 (read01 utils/adt, round 35) — uuid.c + xml.c: the UUID
//! surface (I/O forms, ordering, extract_version/timestamp) confirmed
//! aligned, plus the XMLPARSE(DOCUMENT|CONTENT expr) syntax. Byte-locked
//! vs PG18.

use spg_engine::{Engine, QueryResult};

fn row_of(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap() {
        QueryResult::Rows { rows, .. } => rows[0]
            .values
            .iter()
            .map(spg_engine::eval::value_to_text)
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err_of(e: &mut Engine, sql: &str) -> String {
    format!("{}", e.execute(sql).unwrap_err())
}

#[test]
fn uuid_io_and_extract() {
    let mut e = Engine::new();
    // Braced and unhyphenated input forms canonicalize; case-insensitive.
    assert_eq!(
        row_of(
            &mut e,
            "SELECT '{550e8400-e29b-41d4-a716-446655440000}'::uuid, \
             '550E8400E29B41D4A716446655440000'::uuid, \
             '550e8400-e29b-41d4-a716-446655440000'::uuid = \
             '550E8400-E29B-41D4-A716-446655440000'::uuid"
        ),
        vec![
            "550e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "true"
        ]
    );
    assert_eq!(
        row_of(
            &mut e,
            "SELECT uuid_extract_version('550e8400-e29b-41d4-a716-446655440000'::uuid), \
             uuid_extract_version('018f6d0e-0000-7000-8000-000000000000'::uuid)"
        ),
        vec!["4", "7"]
    );
    assert!(
        err_of(
            &mut e,
            "SELECT 'zzze8400-e29b-41d4-a716-446655440000'::uuid"
        )
        .contains("invalid input syntax for type uuid")
    );
}

#[test]
fn xmlparse_document_and_content() {
    let mut e = Engine::new();
    assert_eq!(
        row_of(
            &mut e,
            "SELECT xmlparse(document '<a>1</a>'), xmlparse(content 'foo<b/>')"
        ),
        vec!["<a>1</a>", "foo<b/>"]
    );
    // xmlelement still parses (regression guard for the shared branch).
    assert_eq!(
        row_of(&mut e, "SELECT xmlelement(name foo, 'bar')"),
        vec!["<foo>bar</foo>"]
    );
}
