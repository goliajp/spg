//! v7.39 (round 291) — `name`, PG's identifier type, as a type of its
//! own rather than an alias for text.
//!
//! `CREATE TABLE t (a name)` answered `type "name" does not exist` —
//! a capability wall on legal SQL, and the shape pg_dump emits for
//! anything modelled on a catalog. `pg_typeof('abc'::name)` said
//! `text`, and `format_type` on such a column said `???`.
//!
//! The 63-byte truncation was already right; only the type identity
//! was missing. `name` values are stored as `Value::Text`, so the
//! SCHEMA is the only witness of the type — the value can never be
//! one, which is why `pg_typeof` had to learn to read the declared
//! type here the way it already does for a composite column.
//!
//! On-disk: DataType tag 72. No FILE_VERSION bump — an older image
//! cannot contain a tag that did not exist, so nothing already written
//! moves or needs migrating.
//!
//! Every expectation was read off live PG 18.4.

use spg_engine::{Engine, QueryResult};

fn one(e: &mut Engine, sql: &str) -> String {
    let r = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    let QueryResult::Rows { rows, .. } = r else {
        panic!("expected Rows from {sql}");
    };
    rows[0]
        .values
        .iter()
        .map(spg_engine::eval::value_to_text)
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn a_name_column_can_be_declared() {
    // The capability wall: this was `type "name" does not exist`.
    let mut e = Engine::new();
    e.execute("CREATE TABLE nmt (a name)").unwrap();
    e.execute("INSERT INTO nmt VALUES ('hello')").unwrap();
    assert_eq!(
        one(&mut e, "SELECT a, pg_typeof(a), length(a) FROM nmt"),
        "hello|name|5"
    );
}

#[test]
fn the_cast_reports_its_own_type() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT pg_typeof('abc'::name)"), "name");
    assert_eq!(one(&mut e, "SELECT 'abc'::name"), "abc");
    assert_eq!(one(&mut e, "SELECT length('abc'::name)"), "3");
}

#[test]
fn it_compares_and_concatenates_as_text_does() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT 'abc'::name = 'abc'::text"), "true");
    assert_eq!(one(&mut e, "SELECT 'abc'::name < 'abd'::name"), "true");
    assert_eq!(one(&mut e, "SELECT 'Abc'::name = 'abc'::name"), "false");
    assert_eq!(one(&mut e, "SELECT 'abc'::name || 'def'"), "abcdef");
    // Concatenation yields text, not name — PG's operator resolution.
    assert_eq!(
        one(&mut e, "SELECT pg_typeof('abc'::name || 'def')"),
        "text"
    );
    assert_eq!(one(&mut e, "SELECT pg_typeof(upper('abc'::name))"), "text");
}

#[test]
fn it_truncates_at_namedatalen_minus_one() {
    let mut e = Engine::new();
    assert_eq!(one(&mut e, "SELECT length(repeat('x', 100)::name)"), "63");
    // …and a column truncates on the way in, silently, as PG does.
    e.execute("CREATE TABLE nmt2 (a name)").unwrap();
    e.execute("INSERT INTO nmt2 SELECT repeat('y', 100)")
        .unwrap();
    assert_eq!(one(&mut e, "SELECT length(a) FROM nmt2"), "63");
}

#[test]
fn format_type_names_it() {
    let mut e = Engine::new();
    e.execute("CREATE TABLE nmt3 (a name)").unwrap();
    assert_eq!(
        one(
            &mut e,
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'nmt3'::regclass AND attname = 'a'",
        ),
        "name",
    );
}

#[test]
fn the_column_type_survives_a_catalog_round_trip() {
    // Tag 72 on disk; a reload that lost it would report text.
    let mut e = Engine::new();
    e.execute("CREATE TABLE nmt4 (a name)").unwrap();
    e.execute("INSERT INTO nmt4 VALUES ('keep')").unwrap();
    let bytes = e.catalog().serialize();
    let mut restored = Engine::restore_envelope(&bytes).expect("reload");
    assert_eq!(
        one(&mut restored, "SELECT a, pg_typeof(a) FROM nmt4"),
        "keep|name",
    );
}
