//! 9.0.0 — two catalog rows shared one oid, and `pg_dump` stopped on it.
//!
//! An oid is a row's identity: `pg_dump` prepares `dumpFunc($1)` and
//! `dumpCompositeType($1)` and executes one per object, and a catalog
//! that answers TWO rows for the oid it was given fails the whole dump
//! with `query returned 2 rows instead of one`. There is no wrong answer
//! to look at — the dump simply does not happen.
//!
//! Two of them, both measured 2026-09-21 on the wire:
//!
//!   * `pg_proc` — `show_trgm` and `xmlforest` were both 900086, and
//!     `word_similarity` and `isnull` both 900087. The pg_trgm family
//!     had been numbered into a band that was already full.
//!   * `pg_type` — `character_data` and `yes_or_no` were both 14543,
//!     because the domain's oid was derived from its BASE type and both
//!     are domains over `character varying`. A base type does not
//!     identify a domain.
//!
//! Neither was visible while a separate defect stopped `pg_dump` at its
//! first composite type. This test asks every catalog that publishes an
//! oid, so the next one cannot hide behind the one before it.

use spg_engine::{Engine, QueryResult};

fn vals(e: &mut Engine, sql: &str) -> Vec<String> {
    match e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}")) {
        QueryResult::Rows { rows, .. } => rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(spg_engine::eval::value_to_text)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn no_catalog_answers_two_rows_for_one_oid() {
    let mut e = Engine::new();
    // Something for the per-relation catalogs to have rows about, so
    // the floor below is a floor and not a vacuous pass.
    for sql in [
        "CREATE TABLE oiduniq(a int PRIMARY KEY, b text UNIQUE, c int CHECK (c > 0))",
        "CREATE INDEX oiduniq_c ON oiduniq (c)",
        "CREATE EXTENSION pg_trgm",
    ] {
        e.execute(sql).unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    // Every catalog this engine publishes an `oid` column for.
    let catalogs = [
        "pg_proc",
        "pg_type",
        "pg_class",
        "pg_operator",
        "pg_opclass",
        "pg_am",
        "pg_namespace",
        "pg_constraint",
        // `pg_index` is keyed on `indexrelid`, not `oid` — PostgreSQL
        // has no `oid` column there either.
        "pg_database",
        "pg_collation",
        "pg_extension",
    ];
    for c in catalogs {
        // The floor: a catalog that answers NO rows cannot fail this,
        // and would be the instrument failing rather than the catalog
        // passing.
        let total = vals(&mut e, &format!("SELECT count(*) FROM {c}"));
        assert_ne!(total, vec!["0"], "{c} is empty — nothing was checked");
        let dups = vals(
            &mut e,
            &format!(
                "SELECT oid FROM (SELECT oid FROM {c} GROUP BY oid HAVING count(*) > 1) d \
                 ORDER BY 1"
            ),
        );
        assert!(dups.is_empty(), "{c} answers two rows for oid(s) {dups:?}");
    }
}
