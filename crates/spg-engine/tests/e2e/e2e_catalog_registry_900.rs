//! 9.0.0 (N19) — one list of catalog relations, and every surface reads it.
//!
//! The set was written in six places: the parser gate that decides
//! whether `pg_catalog.x` is rewritten (107 names), the `pg_class` row
//! builder (43), the empty-catalog table (30), the meta-view dispatch
//! (17), a catalog-oid table (64) and a hand-copied 13-name subset in
//! the evaluator. Three of those are gone; the two that remain hold
//! COLUMNS and BODIES rather than the set, and this pins that every
//! name they mention is in the registry.
//!
//! The disagreement was observable, which is why it is a defect and not
//! a tidiness complaint. Measured on SPG before the change:
//!
//! ```text
//!   SELECT count(*) FROM pg_database                        1
//!   SELECT count(*) FROM pg_class WHERE relname='pg_database'   0
//! ```
//!
//! PostgreSQL 18.6 answers 7 and 1. Sixty-four names answered a query
//! and had no `pg_class` row at all, so `information_schema` could not
//! list them and a reflection tool asking what the database holds was
//! told they do not exist.

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
fn every_catalog_relation_the_parser_admits_has_a_pg_class_row() {
    let mut e = Engine::new();
    let mut missing: Vec<&str> = Vec::new();
    for (name, _, _, _) in spg_sql::catalog_registry::CATALOG_RELATIONS {
        let got = col(
            &mut e,
            &format!("SELECT count(*) FROM pg_class WHERE relname = '{name}'"),
        );
        if got.first().map(String::as_str) != Some("1") {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "{} relations answer a query and have no pg_class row: {missing:?}",
        missing.len()
    );
}

#[test]
fn the_registry_carries_postgresqls_own_oids() {
    // The oids are a contract: a client caches `'pg_type'::regclass` and
    // expects it to keep meaning pg_type. Measured on PG 18.6.
    let mut e = Engine::new();
    for (name, oid) in [
        ("pg_type", "1247"),
        ("pg_attribute", "1249"),
        ("pg_proc", "1255"),
        ("pg_class", "1259"),
        ("pg_database", "1262"),
        ("pg_constraint", "2606"),
        ("pg_ts_config", "3602"),
    ] {
        assert_eq!(
            col(&mut e, &format!("SELECT '{name}'::regclass::oid")),
            vec![oid.to_string()],
            "{name}"
        );
        assert_eq!(
            col(
                &mut e,
                &format!("SELECT oid FROM pg_class WHERE relname = '{name}'")
            ),
            vec![oid.to_string()],
            "{name} through pg_class"
        );
    }
}

#[test]
fn a_relation_that_answers_reports_the_kind_postgresql_gives_it() {
    // `pg_stats` is a VIEW on PG 18.6, `pg_type` a table. The registry
    // carries the kind, so the two cannot drift apart.
    let mut e = Engine::new();
    for (name, kind) in [("pg_type", "r"), ("pg_stats", "v"), ("pg_views", "v")] {
        assert_eq!(
            col(
                &mut e,
                &format!("SELECT relkind FROM pg_class WHERE relname = '{name}'")
            ),
            vec![kind.to_string()],
            "{name}"
        );
    }
}

#[test]
fn a_relation_that_answers_under_its_own_name_is_not_rewritten() {
    // `rewrite` is the parser's question, not the catalog's. These four
    // answer through `meta_view_result` under their bare name, so
    // rewriting `pg_catalog.pg_locks` to `__spg_pg_locks` would
    // mis-target the lookup — and they still need the `pg_class` row,
    // which is the half that was missing.
    for name in ["pg_locks", "pg_stat_activity", "pg_statio_user_tables"] {
        assert!(
            spg_sql::catalog_registry::is_catalog_relation(name),
            "{name} answers and is in no registry"
        );
        assert!(
            !spg_sql::catalog_registry::is_rewritten_catalog(name),
            "{name} must keep its own name"
        );
        let mut e = Engine::new();
        assert_eq!(
            col(
                &mut e,
                &format!("SELECT count(*) FROM pg_class WHERE relname = '{name}'")
            ),
            vec!["1".to_string()],
            "{name}"
        );
        // And it still answers, through the path it always took —
        // BARE and QUALIFIED both. The qualified spelling is the one
        // the `rewrite` flag decides: rewriting `pg_catalog.pg_locks`
        // to `__spg_pg_locks` names a view nothing materialises, so
        // the query fails. Measured on PG 18.6: both spellings answer.
        e.execute(&format!("SELECT count(*) FROM {name}"))
            .unwrap_or_else(|err| panic!("bare {name}: {err:?}"));
        e.execute(&format!("SELECT count(*) FROM pg_catalog.{name}"))
            .unwrap_or_else(|err| panic!("qualified {name}: {err:?}"));
    }
}
