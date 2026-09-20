//! 9.0.0 — ten catalog relations answered queries and appeared in no
//! catalog.
//!
//! `SELECT * FROM pg_views` worked; `SELECT … FROM pg_class WHERE
//! relname='pg_views'` found nothing. The same for `pg_tables`,
//! `pg_indexes`, `pg_matviews`, `pg_sequences`, `pg_settings`,
//! `pg_roles`, `pg_user`, `pg_prepared_statements` and `pg_rules` — so
//! `pg_attribute` described none of their columns and
//! `information_schema` could not list them at all: measured,
//! `information_schema.columns WHERE table_name='pg_class'` answered 0
//! rows where PostgreSQL 18.6 answers 34.
//!
//! `pg_stats`, which WAS listed, reported `relkind 'r'` where PG says
//! `'v'`. The list carries each relation's kind now.
//!
//! The set of catalogs SPG answers for is still written in three
//! places — this list, the `__spg_pg_*` dispatch and `META_VIEWS` —
//! and that is what let ten of them fall out of one of the three.

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

/// Every relation that answers a query is in `pg_class`, with the kind
/// PostgreSQL 18.6 reports for it.
#[test]
fn a_relation_that_answers_is_in_the_catalog() {
    let mut e = Engine::new();
    for (name, kind) in [
        ("pg_class", "r"),
        ("pg_type", "r"),
        ("pg_stats", "v"),
        ("pg_views", "v"),
        ("pg_tables", "v"),
        ("pg_indexes", "v"),
        ("pg_matviews", "v"),
        ("pg_sequences", "v"),
        ("pg_settings", "v"),
        ("pg_roles", "v"),
        ("pg_user", "v"),
        ("pg_prepared_statements", "v"),
        ("pg_rules", "v"),
    ] {
        // It answers…
        let _ = e
            .execute(&format!("SELECT count(*) FROM {name}"))
            .unwrap_or_else(|err| panic!("{name} does not answer: {err:?}"));
        // …and the catalog says so, with the right kind.
        assert_eq!(
            col(
                &mut e,
                &format!("SELECT relkind::text FROM pg_class WHERE relname='{name}'")
            ),
            vec![kind.to_string()],
            "{name}"
        );
    }
}

#[test]
fn information_schema_lists_the_catalogs() {
    let mut e = Engine::new();
    // PostgreSQL 18.6 answers 34 for pg_class's column count here.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM information_schema.columns WHERE table_name='pg_class'"
        ),
        vec!["34".to_string()]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT table_schema||' '||table_type FROM information_schema.tables \
             WHERE table_name='pg_class'"
        ),
        vec!["pg_catalog BASE TABLE".to_string()]
    );
    assert_eq!(
        col(
            &mut e,
            "SELECT table_type FROM information_schema.tables WHERE table_name='pg_views'"
        ),
        vec!["VIEW".to_string()]
    );
    // And a user table is still listed under `public`.
    e.execute("CREATE TABLE u (a int)").unwrap();
    assert_eq!(
        col(
            &mut e,
            "SELECT table_schema FROM information_schema.tables WHERE table_name='u'"
        ),
        vec!["public".to_string()]
    );
}

#[test]
fn every_listed_catalog_describes_its_own_columns() {
    let mut e = Engine::new();
    // The shape the ten used to fail: a catalog with a pg_class row but
    // no pg_attribute rows would be published and not self-described.
    assert_eq!(
        col(
            &mut e,
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname='pg_catalog' AND NOT EXISTS ( \
               SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid)"
        ),
        vec!["0".to_string()]
    );
}
