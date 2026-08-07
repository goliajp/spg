//! v7.37.42-T2 ζ-B — CREATE TYPE … AS (…) composite-type
//! meta-system: catalog registry, DDL round-trip, column binding,
//! DROP TYPE, snapshot persistence.
//!
//! The composite registry parallels the ENUM / DOMAIN registries
//! shipped in v7.17.0. CREATE TYPE … AS (…) stores the ordered
//! `(field_name, field_type)` list in `catalog.composite_types`
//! so sentori / mailrs PG dumps that emit composite definitions
//! survive a snapshot round-trip and can be referenced by columns.
//! Composite columns store as JSONB at the storage tier — the
//! json path operators (`->`, `->>`, `jsonb_each`) give the
//! field-access surface PG composites need without a recursive
//! Value::Composite variant.

use spg_engine::Engine;

#[test]
fn create_composite_type_registers_in_catalog() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (street TEXT, city TEXT, zip INT)")
        .unwrap();
    let comp = e
        .catalog()
        .composite_types()
        .get("addr")
        .expect("composite type stored");
    assert_eq!(comp.name, "addr");
    assert_eq!(comp.fields.len(), 3);
    assert_eq!(comp.fields[0].0, "street");
    assert_eq!(comp.fields[0].1, spg_storage::DataType::Text);
    assert_eq!(comp.fields[2].0, "zip");
    assert_eq!(comp.fields[2].1, spg_storage::DataType::Int);
}

#[test]
fn create_composite_with_single_field_accepted() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE wrap AS (val INT)").unwrap();
    assert!(e.catalog().composite_types().contains_key("wrap"));
}

#[test]
fn create_composite_with_no_fields_is_accepted() {
    // v7.39 (round 769, F31 tranche 5 #140) — PG18-measured: an
    // attribute-less composite is LEGAL (`CREATE TYPE x AS ()`
    // succeeds there; the old note claimed PG requires a field).
    let mut e = Engine::new();
    e.execute("CREATE TYPE empty AS ()").unwrap();
    e.execute("DROP TYPE empty").unwrap();
}

#[test]
fn duplicate_create_composite_type_errors() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (street TEXT)").unwrap();
    let err = e.execute("CREATE TYPE addr AS (city TEXT)");
    assert!(err.is_err(), "duplicate CREATE TYPE should error");
}

#[test]
fn duplicate_field_name_in_composite_rejected() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TYPE bad AS (a INT, a TEXT)");
    assert!(
        err.is_err(),
        "duplicate composite field names should be rejected per PG"
    );
}

#[test]
fn case_insensitive_duplicate_field_rejected() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TYPE bad AS (id INT, ID TEXT)");
    assert!(
        err.is_err(),
        "PG composite field names are case-insensitive duplicates"
    );
}

#[test]
fn composite_collides_with_existing_enum_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE shared AS ENUM ('a', 'b')").unwrap();
    let err = e.execute("CREATE TYPE shared AS (x INT)");
    assert!(
        err.is_err(),
        "composite name colliding with enum should fail"
    );
}

#[test]
fn composite_collides_with_existing_domain_rejected() {
    let mut e = Engine::new();
    e.execute("CREATE DOMAIN shared AS INT").unwrap();
    let err = e.execute("CREATE TYPE shared AS (x INT)");
    assert!(
        err.is_err(),
        "composite name colliding with domain should fail"
    );
}

#[test]
fn drop_composite_type_removes_it() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (city TEXT)").unwrap();
    e.execute("DROP TYPE addr").unwrap();
    // Re-create allowed.
    e.execute("CREATE TYPE addr AS (zip INT)").unwrap();
    let comp = e.catalog().composite_types().get("addr").unwrap();
    assert_eq!(comp.fields[0].0, "zip");
}

#[test]
fn drop_composite_type_if_exists_silent_on_missing() {
    let mut e = Engine::new();
    e.execute("DROP TYPE IF EXISTS not_there").unwrap();
}

#[test]
fn drop_unknown_type_errors_without_if_exists() {
    let mut e = Engine::new();
    let err = e.execute("DROP TYPE ghost");
    assert!(err.is_err(), "missing type drop without IF EXISTS errors");
}

#[test]
fn composite_used_as_column_type_takes_a_record_literal() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (street TEXT, zip INT)")
        .unwrap();
    e.execute("CREATE TABLE customers (id INT NOT NULL, home addr)")
        .unwrap();
    // v7.39 (round 263) — this used to insert a JSON-OBJECT literal and
    // assert it round-tripped, which asserted SPG's internal storage
    // form rather than PG's surface: PG refuses that text with
    // `malformed record literal … Missing left parenthesis` (probed),
    // and SPG now does too, because a value written into a composite
    // column is relabelled through the declared type first.
    let err = e.execute(r#"INSERT INTO customers VALUES (1, '{"street": "Main", "zip": 90210}')"#);
    assert!(
        format!("{err:?}").contains("malformed record literal"),
        "{err:?}"
    );
    // The record literal is the accepted spelling, and it round-trips.
    e.execute(r#"INSERT INTO customers VALUES (1, '("Main",90210)')"#)
        .unwrap();
    let r = e.execute("SELECT home FROM customers ORDER BY id").unwrap();
    let rows = match r {
        spg_engine::QueryResult::Rows { rows, .. } => rows,
        _ => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(
        spg_engine::eval::value_to_text(&rows[0].values[0]),
        "(Main,90210)"
    );
}

#[test]
fn unknown_composite_type_ident_in_create_table_rejected() {
    let mut e = Engine::new();
    let err = e.execute("CREATE TABLE t (id INT NOT NULL, x my_unknown_ct)");
    assert!(
        err.is_err(),
        "column type that's not a builtin/ENUM/DOMAIN/composite errors"
    );
}

#[test]
fn composite_type_round_trips_catalog_snapshot() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE addr AS (street TEXT, city TEXT, zip INT)")
        .unwrap();
    // FILE_VERSION 52 bump exercises the new composite block.
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).expect("round-trip");
    let comp = restored
        .composite_types()
        .get("addr")
        .expect("composite persisted");
    assert_eq!(comp.fields.len(), 3);
    assert_eq!(comp.fields[0].0, "street");
    assert_eq!(comp.fields[0].1, spg_storage::DataType::Text);
    assert_eq!(comp.fields[1].0, "city");
    assert_eq!(comp.fields[1].1, spg_storage::DataType::Text);
    assert_eq!(comp.fields[2].0, "zip");
    assert_eq!(comp.fields[2].1, spg_storage::DataType::Int);
}

#[test]
fn composite_with_many_data_types_round_trips() {
    let mut e = Engine::new();
    e.execute(
        "CREATE TYPE wide AS (a INT, b BIGINT, c TEXT, d BOOL, \
                 e FLOAT, f UUID, g DATE)",
    )
    .unwrap();
    let snapshot = e.catalog().serialize();
    let restored = spg_storage::Catalog::deserialize(&snapshot).unwrap();
    let comp = restored.composite_types().get("wide").unwrap();
    assert_eq!(comp.fields.len(), 7);
    assert_eq!(comp.fields[0].1, spg_storage::DataType::Int);
    assert_eq!(comp.fields[1].1, spg_storage::DataType::BigInt);
    assert_eq!(comp.fields[5].1, spg_storage::DataType::Uuid);
    assert_eq!(comp.fields[6].1, spg_storage::DataType::Date);
}

#[test]
fn drop_unrelated_composite_keeps_others() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE a AS (x INT)").unwrap();
    e.execute("CREATE TYPE b AS (y TEXT)").unwrap();
    e.execute("CREATE TYPE c AS (z BIGINT)").unwrap();
    e.execute("DROP TYPE b").unwrap();
    assert!(e.catalog().composite_types().contains_key("a"));
    assert!(!e.catalog().composite_types().contains_key("b"));
    assert!(e.catalog().composite_types().contains_key("c"));
}

#[test]
fn empty_catalog_has_no_composite_types() {
    let e = Engine::new();
    assert!(e.catalog().composite_types().is_empty());
}

#[test]
fn composite_field_data_types_preserved() {
    let mut e = Engine::new();
    e.execute("CREATE TYPE mixed AS (id BIGINT, name VARCHAR(64), price NUMERIC(10, 2))")
        .unwrap();
    let comp = e.catalog().composite_types().get("mixed").unwrap();
    assert_eq!(comp.fields.len(), 3);
    assert_eq!(comp.fields[0].1, spg_storage::DataType::BigInt);
    // Varchar / Numeric carry their parameters through column_type_to_data_type.
    assert!(matches!(
        comp.fields[1].1,
        spg_storage::DataType::Varchar(64)
    ));
    assert!(matches!(
        comp.fields[2].1,
        spg_storage::DataType::Numeric {
            precision: 10,
            scale: 2
        }
    ));
}
