//! read01 round 330 (V48) — the information_schema domain family.
//!
//! PG builds `information_schema` out of four domains, and its columns are
//! declared over them, so `pg_typeof` names the DOMAIN rather than the
//! base type. SPG reported the base spelling (`text`, `integer`), and the
//! domains did not exist anywhere — a client resolving the type behind an
//! information_schema column found nothing in `pg_type` either.
//!
//! Measured on PG 18.4:
//!
//! | expression | PG |
//! |---|---|
//! | `pg_typeof(table_name)` from `information_schema.tables` | `information_schema.sql_identifier` |
//! | `pg_typeof(table_type)` | `information_schema.character_data` |
//! | `pg_typeof(is_nullable)` from `…columns` | `information_schema.yes_or_no` |
//! | `pg_typeof(ordinal_position)` | `information_schema.cardinal_number` |
//! | `pg_type` for the four names | `typtype = 'd'`, over name / varchar / integer |
//!
//! They are deliberately NOT catalog domains: a catalog domain is user
//! data — it would serialise into the snapshot and a dump would emit
//! `CREATE DOMAIN` for it. They are built into the server.

use spg_engine::Engine;
use spg_storage::Value;

fn cells(e: &mut Engine, sql: &str) -> Vec<Vec<Value<'static>>> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        spg_engine::QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| r.values.iter().cloned().map(Value::into_owned).collect())
            .collect(),
        other => panic!("`{sql}` did not return rows: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE v48 (id INT NOT NULL, nm TEXT)").unwrap();
    e
}

#[test]
fn information_schema_tables_columns_report_their_domains() {
    let mut e = fixture();
    assert_eq!(
        cells(
            &mut e,
            "SELECT pg_typeof(table_name), pg_typeof(table_schema), pg_typeof(table_type) \
             FROM information_schema.tables WHERE table_name = 'v48'"
        ),
        vec![vec![
            Value::text("information_schema.sql_identifier"),
            Value::text("information_schema.sql_identifier"),
            Value::text("information_schema.character_data"),
        ]],
    );
}

#[test]
fn information_schema_columns_report_their_domains() {
    let mut e = fixture();
    assert_eq!(
        cells(
            &mut e,
            "SELECT pg_typeof(column_name), pg_typeof(is_nullable), \
             pg_typeof(ordinal_position), pg_typeof(data_type) \
             FROM information_schema.columns WHERE table_name = 'v48' LIMIT 1"
        ),
        vec![vec![
            Value::text("information_schema.sql_identifier"),
            Value::text("information_schema.yes_or_no"),
            Value::text("information_schema.cardinal_number"),
            Value::text("information_schema.character_data"),
        ]],
    );
}

/// The VALUES are unchanged — this is a type-identity fix, and the
/// domains' base types are what the columns still hold.
#[test]
fn the_values_are_unchanged() {
    let mut e = fixture();
    assert_eq!(
        cells(
            &mut e,
            "SELECT column_name, is_nullable, ordinal_position \
             FROM information_schema.columns WHERE table_name = 'v48' ORDER BY ordinal_position"
        ),
        vec![
            vec![Value::text("id"), Value::text("NO"), Value::Int(1)],
            vec![Value::text("nm"), Value::text("YES"), Value::Int(2)],
        ],
    );
}

/// A client resolving the type behind one of those columns finds it.
#[test]
fn the_domains_exist_in_pg_type() {
    let mut e = fixture();
    assert_eq!(
        cells(
            &mut e,
            "SELECT typname, typtype FROM pg_type \
             WHERE typname IN ('sql_identifier', 'yes_or_no', 'cardinal_number', \
             'character_data') ORDER BY typname"
        ),
        vec![
            vec![Value::text("cardinal_number"), Value::text("d")],
            vec![Value::text("character_data"), Value::text("d")],
            vec![Value::text("sql_identifier"), Value::text("d")],
            vec![Value::text("yes_or_no"), Value::text("d")],
        ],
    );
}

/// They must NOT be catalog domains — that is user data, and a dump would
/// emit `CREATE DOMAIN` for them.
#[test]
fn the_domains_are_not_user_catalog_objects() {
    let mut e = fixture();
    let rows = cells(
        &mut e,
        "SELECT domain_name FROM information_schema.domains ORDER BY domain_name",
    );
    assert!(
        rows.is_empty(),
        "a database with no user domains must list none: {rows:?}"
    );
}
