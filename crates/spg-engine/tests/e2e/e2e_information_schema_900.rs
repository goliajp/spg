//! 9.0.0 (N23 / N20) — `information_schema` answers, and lists itself.
//!
//! Asked for each of PostgreSQL 18.6's 69 relations by name, **52
//! answered `meta view … is not yet materialisable`** — an ERROR where
//! PostgreSQL answers rows or an empty set. Among them
//! `data_type_privileges`, `collations`, `user_defined_types`,
//! `routine_privileges` and the whole `role_*_grants` family. Every ORM
//! reflects through this schema.
//!
//! And it did not list ITSELF: PG 18.6's `information_schema.columns`
//! covers nine schemas including `information_schema` (696 columns),
//! while SPG covered `public` and `pg_catalog` only, so a tool asking
//! what the database holds was told this schema holds nothing.
//!
//! Measured on PG 18.6 and matched here:
//!
//! ```text
//!   sql_features                    755 rows, byte-identical
//!   sql_sizing                       23
//!   sql_parts                        11
//!   sql_implementation_info          12
//!   character_sets                    1
//!   information_schema_catalog_name   1
//!   foreign_tables / transforms / user_mappings …   0, with columns
//!   information_schema's own columns   696
//! ```

use spg_engine::{Engine, QueryResult};

fn col(e: &mut Engine, sql: &str) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = e
        .execute(sql)
        .unwrap_or_else(|err| panic!("{sql}: {err:?}"))
    else {
        panic!("expected rows for {sql}");
    };
    rows.into_iter()
        .map(|r| spg_engine::eval::value_to_text(r.values.first().expect("one column")))
        .collect()
}

#[test]
fn the_fixed_content_relations_carry_postgresqls_own_rows() {
    let mut e = Engine::new();
    for (rel, n) in [
        ("sql_features", "755"),
        ("sql_sizing", "23"),
        ("sql_parts", "11"),
        ("sql_implementation_info", "12"),
        ("character_sets", "1"),
        ("information_schema_catalog_name", "1"),
    ] {
        assert_eq!(
            col(
                &mut e,
                &format!("SELECT count(*) FROM information_schema.{rel}")
            ),
            vec![n.to_string()],
            "{rel}"
        );
    }
}

#[test]
fn a_relation_spg_is_empty_of_answers_with_columns_and_no_rows() {
    // An ERROR is the wrong answer: PostgreSQL answers zero rows for
    // these on an ordinary database too, and a reflection tool reads
    // "no foreign tables", not "this database is broken".
    let mut e = Engine::new();
    for rel in [
        "foreign_tables",
        "foreign_servers",
        "foreign_data_wrappers",
        "user_mappings",
        "transforms",
        "column_options",
        "_pg_foreign_tables",
    ] {
        assert_eq!(
            col(
                &mut e,
                &format!("SELECT count(*) FROM information_schema.{rel}")
            ),
            vec!["0".to_string()],
            "{rel}"
        );
    }
    // And the columns are there to be selected by name.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema='information_schema' AND table_name='foreign_tables'"
        ),
        vec!["5".to_string()]
    );
}

#[test]
fn information_schema_lists_its_own_relations() {
    let mut e = Engine::new();
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'information_schema'"
        ),
        vec!["696".to_string()],
        "PostgreSQL 18.6 answers 696"
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = 'information_schema'"
        ),
        vec!["69".to_string()]
    );
    // Every one of them is a VIEW there, including the `_pg_*` helpers.
    assert_eq!(
        col(
            &mut e,
            "SELECT DISTINCT table_type FROM information_schema.tables \
             WHERE table_schema = 'information_schema'"
        ),
        vec!["VIEW".to_string()]
    );
}

#[test]
fn every_relation_postgresql_has_answers() {
    // The headline: 52 of PG 18.6's 69 relations answered
    // `meta view … is not yet materialisable`, which is an ERROR where
    // PostgreSQL answers rows or an empty set.
    let mut e = seeded_for_reflection();
    let mut refused: Vec<String> = Vec::new();
    for rel in spg_engine::info_schema_relation_names_for_test() {
        if e.execute(&format!("SELECT count(*) FROM information_schema.{rel}"))
            .is_err()
        {
            refused.push(rel.to_string());
        }
    }
    assert!(
        refused.is_empty(),
        "{} of 69 still refuse: {refused:?}",
        refused.len()
    );
}

#[test]
fn the_relations_derived_from_the_catalog_match_postgresql() {
    // Measured on PG 18.6 over the same shapes.
    let mut e = seeded_for_reflection();
    // A domain's base type, named as PostgreSQL names it (`int4`).
    assert_eq!(
        col(
            &mut e,
            "SELECT udt_name FROM information_schema.domain_udt_usage WHERE domain_name='isf_d'"
        ),
        vec!["int4".to_string()]
    );
    // A column's own type, same naming.
    assert_eq!(
        col(
            &mut e,
            "SELECT udt_name FROM information_schema.column_udt_usage              WHERE table_name='isf_a' AND column_name='name'"
        ),
        vec!["varchar".to_string()]
    );
    // An array column's ELEMENT type, keyed by attnum the way PG keys it.
    assert_eq!(
        col(
            &mut e,
            "SELECT dtd_identifier FROM information_schema.element_types              WHERE object_name='et' AND data_type='text'"
        ),
        vec!["a2".to_string()]
    );
    // A view's tables, and the constraint's own name — the name
    // `pg_constraint` gives it, not a second spelling.
    assert_eq!(
        col(
            &mut e,
            "SELECT table_name FROM information_schema.view_table_usage              WHERE view_name='isf_v' ORDER BY table_name"
        ),
        vec!["isf_a".to_string(), "isf_b".to_string()]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT constraint_name FROM information_schema.constraint_table_usage              WHERE constraint_name LIKE '%fkey'"
        ),
        vec!["isf_b_a_id_fkey".to_string()]
    );
    // A generated column and what it reads.
    assert_eq!(
        col(
            &mut e,
            "SELECT column_name FROM information_schema.column_column_usage              WHERE table_name='gcol'"
        ),
        vec!["a".to_string()]
    );
}

#[test]
fn a_function_reflects_through_parameters_and_privileges() {
    let mut e = seeded_for_reflection();
    assert_eq!(
        col(
            &mut e,
            "SELECT parameter_name FROM information_schema.parameters ORDER BY ordinal_position"
        ),
        vec!["x".to_string(), "y".to_string()]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT udt_name FROM information_schema.parameters ORDER BY ordinal_position"
        ),
        vec!["int4".to_string(), "text".to_string()]
    );
    // EXECUTE, once for the owner with the grant option and once for
    // PUBLIC without it — PostgreSQL's default grant.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM information_schema.routine_privileges              WHERE routine_name='f1'"
        ),
        vec!["2".to_string()]
    );
}

#[test]
fn a_column_reports_the_owners_implicit_privileges() {
    // PG lists the owner's four column-level privileges; SPG listed
    // explicit column GRANTs alone, so an un-granted database answered
    // zero rows here.
    let mut e = seeded_for_reflection();
    assert_eq!(
        col(
            &mut e,
            "SELECT DISTINCT privilege_type FROM information_schema.column_privileges              ORDER BY privilege_type"
        ),
        vec![
            "INSERT".to_string(),
            "REFERENCES".to_string(),
            "SELECT".to_string(),
            "UPDATE".to_string()
        ]
    );
}

fn seeded_for_reflection() -> Engine {
    let mut e = Engine::new();
    for sql in [
        "CREATE TABLE isf_a (id int PRIMARY KEY, name varchar(20) NOT NULL)",
        "CREATE TABLE isf_b (id int PRIMARY KEY, a_id int REFERENCES isf_a(id))",
        "CREATE VIEW isf_v AS SELECT a.id, a.name FROM isf_a a JOIN isf_b b ON b.a_id = a.id",
        "CREATE DOMAIN isf_d AS int CHECK (VALUE > 0)",
        "CREATE TYPE isf_c AS (a int, b text)",
        "CREATE TABLE et (a int[], b text[], c int)",
        "CREATE TABLE gcol (a int, b int GENERATED ALWAYS AS (a * 2) STORED)",
        "CREATE FUNCTION f1(x int, y text) RETURNS int LANGUAGE sql AS 'select 1'",
    ] {
        e.execute(sql)
            .unwrap_or_else(|err| panic!("{sql}: {err:?}"));
    }
    e
}

#[test]
fn the_catalog_name_follows_the_session_database() {
    let mut e = Engine::new();
    let name = col(
        &mut e,
        "SELECT catalog_name FROM information_schema.information_schema_catalog_name",
    );
    assert_eq!(name, vec!["spg".to_string()]);
}
