//! v7.39 (round 248) — information_schema.columns against live PG18.4
//! (2026-07-19), the reflection surface SQLAlchemy / Alembic / JDBC
//! read. data_type, is_nullable, ordinal_position, column_default
//! (including the serial nextval spelling), numeric precision/scale and
//! table_constraints already matched; the gaps:
//!
//!   * character_maximum_length was NULL even for a declared varchar(n);
//!   * udt_name reported `text` for varchar (PG: `varchar`), char (PG:
//!     `bpchar`) and every array (PG: the underscore-prefixed element
//!     name, `_text`);
//!   * identity_generation, datetime_precision and is_updatable did not
//!     exist at all — a query naming them errored;
//!   * is_identity keyed off auto_increment, so a plain SERIAL reported
//!     YES where PG says NO (identity is GENERATED … AS IDENTITY only).
//!
//! Recorded residuals: BY DEFAULT identity reports NO (SPG only records
//! the ALWAYS flavour in memory; a catalog field is the fix), and
//! table_catalog is the fixed name `spg` (SPG has no per-database name
//! to report — deliberate).

use spg_engine::{Engine, QueryResult};

fn seeded() -> Engine {
    let mut e = Engine::new();
    e.execute(
        "CREATE TABLE ist (id serial PRIMARY KEY, name varchar(40) NOT NULL DEFAULT 'x', \
         score numeric(6,2), tags text[], created timestamptz DEFAULT now(), flag bool)",
    )
    .unwrap();
    e
}

fn rows(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        spg_storage::Value::Null => String::new(),
                        other => spg_engine::eval::value_to_text(other),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn varchar_length_and_udt_names() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name, character_maximum_length FROM information_schema.columns \
             WHERE table_name='ist' AND column_name='name'"
        ),
        ["name|40"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name, udt_name FROM information_schema.columns \
             WHERE table_name='ist' ORDER BY ordinal_position"
        ),
        [
            "id|int4",
            "name|varchar",
            "score|numeric",
            "tags|_text",
            "created|timestamptz",
            "flag|bool"
        ]
    );
}

#[test]
fn identity_datetime_updatable_columns_exist() {
    let mut e = seeded();
    // A SERIAL is not identity in PG.
    assert_eq!(
        rows(
            &mut e,
            "SELECT is_identity, identity_generation FROM information_schema.columns \
             WHERE table_name='ist' AND column_name='id'"
        ),
        ["NO|"]
    );
    // A real ALWAYS identity reports both.
    e.execute("CREATE TABLE idt (n int GENERATED ALWAYS AS IDENTITY)")
        .unwrap();
    assert_eq!(
        rows(
            &mut e,
            "SELECT is_identity, identity_generation FROM information_schema.columns \
             WHERE table_name='idt'"
        ),
        ["YES|ALWAYS"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name, datetime_precision FROM information_schema.columns \
             WHERE table_name='ist' AND column_name='created'"
        ),
        ["created|6"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT is_updatable FROM information_schema.columns \
             WHERE table_name='ist' AND column_name='id'"
        ),
        ["YES"]
    );
}

#[test]
fn the_reflection_core_is_unchanged() {
    let mut e = seeded();
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_name, data_type, is_nullable FROM information_schema.columns \
             WHERE table_name='ist' ORDER BY ordinal_position"
        ),
        [
            "id|integer|NO",
            "name|character varying|NO",
            "score|numeric|YES",
            "tags|ARRAY|YES",
            "created|timestamp with time zone|YES",
            "flag|boolean|YES",
        ]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name='ist' AND column_name='id'"
        ),
        ["nextval('ist_id_seq'::regclass)"]
    );
    assert_eq!(
        rows(
            &mut e,
            "SELECT numeric_precision, numeric_scale FROM information_schema.columns \
             WHERE table_name='ist' AND column_name='score'"
        ),
        ["6|2"]
    );
}
