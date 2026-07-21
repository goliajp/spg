//! v7.39 (round 313, V34) — pg_catalog's identifier columns are `name`.
//!
//! `pg_typeof(relname)` answered `text`; PG answers `name`. Every synth
//! view built its identifier columns as Text, so reflection tooling that
//! keys off the reported type saw the wrong one. Round 291 had already
//! made `name` a real type — what was missing was using it here.
//!
//! The list of which columns those are is EXHAUSTIVE and comes from PG
//! 18.4 itself: the columns whose `pg_type.typname` is `name`,
//! intersected with the views this engine synthesises — 60 columns
//! across 36 views. It is deliberately not derived from the column NAME,
//! because that does not work: `pg_config.name`, `pg_cursors.name` and
//! `pg_backend_memory_contexts.name` are all `text` in PG, and every
//! `*namespace` is an oid. A rule would have retyped those too, which is
//! why the counter-cases are pinned alongside.
//!
//! information_schema stays as it was: PG types its identifier columns
//! `information_schema.sql_identifier`, a DOMAIN over name, and its
//! others as further domains (`yes_or_no`, `cardinal_number`). That
//! needs the domains registered in the catalogue — different machinery,
//! recorded as V48.

use spg_engine::{Engine, QueryResult};

fn typeof_of(e: &mut Engine, sql: &str) -> String {
    match e.execute(sql).unwrap_or_else(|x| panic!("{sql}: {x:?}")) {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .map_or_else(|| panic!("{sql}: no rows"), |r| {
                spg_engine::eval::value_to_text(&r.values[0])
            }),
        other => panic!("{sql}: {other:?}"),
    }
}

fn fixture() -> Engine {
    let mut e = Engine::new();
    e.execute("CREATE TABLE t34 (id int PRIMARY KEY, v text)")
        .unwrap();
    e.execute("CREATE VIEW v34 AS SELECT id FROM t34").unwrap();
    e.execute("CREATE INDEX i34 ON t34 (v)").unwrap();
    e
}

#[test]
fn identifier_columns_report_name() {
    let mut e = fixture();
    for (sql, what) in [
        ("SELECT pg_typeof(relname) FROM pg_class LIMIT 1", "pg_class.relname"),
        ("SELECT pg_typeof(typname) FROM pg_type LIMIT 1", "pg_type.typname"),
        (
            "SELECT pg_typeof(attname) FROM pg_attribute LIMIT 1",
            "pg_attribute.attname",
        ),
        (
            "SELECT pg_typeof(nspname) FROM pg_namespace LIMIT 1",
            "pg_namespace.nspname",
        ),
        ("SELECT pg_typeof(proname) FROM pg_proc LIMIT 1", "pg_proc.proname"),
        (
            "SELECT pg_typeof(conname) FROM pg_constraint LIMIT 1",
            "pg_constraint.conname",
        ),
        ("SELECT pg_typeof(rolname) FROM pg_roles LIMIT 1", "pg_roles.rolname"),
        ("SELECT pg_typeof(amname) FROM pg_am LIMIT 1", "pg_am.amname"),
    ] {
        assert_eq!(typeof_of(&mut e, sql), "name", "{what}");
    }
}

/// The user-facing listing views name several columns each, and PG types
/// every one of them `name` — including the ones that do not end in
/// "name" (`tablespace`, `tableowner`).
#[test]
fn the_listing_views_report_name_for_all_of_theirs() {
    let mut e = fixture();
    for (sql, what) in [
        (
            "SELECT pg_typeof(schemaname) FROM pg_tables LIMIT 1",
            "pg_tables.schemaname",
        ),
        (
            "SELECT pg_typeof(tablename) FROM pg_tables LIMIT 1",
            "pg_tables.tablename",
        ),
        (
            "SELECT pg_typeof(tableowner) FROM pg_tables LIMIT 1",
            "pg_tables.tableowner",
        ),
        (
            "SELECT pg_typeof(viewname) FROM pg_views LIMIT 1",
            "pg_views.viewname",
        ),
        (
            "SELECT pg_typeof(indexname) FROM pg_indexes LIMIT 1",
            "pg_indexes.indexname",
        ),
    ] {
        assert_eq!(typeof_of(&mut e, sql), "name", "{what}");
    }
}

/// The half a naming rule would have got wrong. These must NOT be name.
#[test]
fn non_identifier_columns_are_left_alone() {
    let mut e = fixture();
    // PG: text. A view's body is data, not an identifier.
    assert_eq!(
        typeof_of(&mut e, "SELECT pg_typeof(definition) FROM pg_views LIMIT 1"),
        "text"
    );
    assert_eq!(
        typeof_of(&mut e, "SELECT pg_typeof(indexdef) FROM pg_indexes LIMIT 1"),
        "text"
    );
    // A `*namespace` is an oid in PG, and stays an integer here — the
    // point is that it did not become `name`.
    assert_ne!(
        typeof_of(&mut e, "SELECT pg_typeof(relnamespace) FROM pg_class LIMIT 1"),
        "name"
    );
    assert_ne!(
        typeof_of(&mut e, "SELECT pg_typeof(oid) FROM pg_class LIMIT 1"),
        "name"
    );
}

/// Retyping must not disturb what the columns actually hold: they still
/// compare and filter as text.
#[test]
fn a_name_column_still_behaves_like_the_string_it_holds() {
    let mut e = fixture();
    match e
        .execute("SELECT relname FROM pg_class WHERE relname = 't34'")
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "t34");
        }
        other => panic!("{other:?}"),
    }
    // LIKE, ORDER BY and a join across two retyped columns all still work.
    match e
        .execute(
            "SELECT count(*) FROM pg_tables WHERE tablename LIKE 't3%'",
        )
        .unwrap()
    {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(spg_engine::eval::value_to_text(&rows[0].values[0]), "1");
        }
        other => panic!("{other:?}"),
    }
}

/// information_schema is a different type family: PG declares its columns
/// over its own DOMAINS, not over `name`.
///
/// v7.39 (round 330, V48) — this assertion used to read `text` and carried
/// the note "the day V48 lands, this test is what has to change". It
/// landed; the domains are built into the server now.
#[test]
fn information_schema_reports_its_own_domain_family() {
    let mut e = fixture();
    assert_eq!(
        typeof_of(
            &mut e,
            "SELECT pg_typeof(table_name) FROM information_schema.tables LIMIT 1"
        ),
        "information_schema.sql_identifier"
    );
    // …and NOT `name`, which is the pg_catalog family this round 313 test
    // is otherwise about.
    assert_eq!(
        typeof_of(
            &mut e,
            "SELECT pg_typeof(relname) FROM pg_class LIMIT 1"
        ),
        "name"
    );
}
